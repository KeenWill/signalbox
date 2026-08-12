//! Atomic deployment-scoped pending-runner promotion.

use std::{error::Error, fmt};

use rust_decimal::{Decimal, prelude::ToPrimitive};
use signalbox_application::{PromotePendingRunnerOutcome, PromotePendingRunnerTransaction};
use signalbox_domain::{
    DurableCommandId, PromotePendingRunner, PromotePendingRunnerRejection,
    PromotePendingRunnerResult, PromotedRunnerEnrollment, RunnerEnrollmentId,
    RunnerEnrollmentRequestId, RunnerGeneration, RunnerId, RunnerNonLostConnectionState,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{self, CommandKind, PROMOTE_PENDING_RUNNER_KIND, RegistryInspectionError},
    lock_inventory::{
        PROMOTE_PENDING_RUNNER_CONNECTION, PROMOTE_PENDING_RUNNER_ENROLLMENTS,
        PROMOTE_PENDING_RUNNER_REGISTRATION,
    },
    mapping::{
        PromotePendingRunnerRejectionStorageKind, PromotePendingRunnerResultStorageKind,
        durable_command_id_to_uuid, promote_pending_runner_rejection_from_str,
        promote_pending_runner_rejection_to_str, promote_pending_runner_result_from_str,
        promote_pending_runner_result_to_str, runner_connection_state_from_str,
        runner_non_lost_connection_state_from_str, runner_non_lost_connection_state_to_str,
    },
    runner_protocol::RunnerConnectionState,
};

const STORAGE_VERSION: i16 = 1;

/// Database or fail-closed promotion failure.
#[derive(Debug)]
pub enum PromotePendingRunnerRepositoryError {
    InvalidCommandId,
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    Corruption(&'static str),
}

impl fmt::Display for PromotePendingRunnerRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommandId => {
                formatter.write_str("runner promotion command identity is reserved")
            }
            Self::Database(error) => {
                write!(formatter, "runner promotion database failure: {error}")
            }
            Self::CommitAmbiguous(error) => {
                write!(formatter, "runner promotion commit is ambiguous: {error}")
            }
            Self::Corruption(reason) => {
                write!(formatter, "runner promotion storage is corrupt: {reason}")
            }
        }
    }
}

impl Error for PromotePendingRunnerRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::InvalidCommandId | Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for PromotePendingRunnerRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// PostgreSQL transaction for one deployment-scoped promotion command.
#[derive(Clone, Debug)]
pub struct PromotePendingRunnerRepository {
    pool: PgPool,
}

impl PromotePendingRunnerRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn handle(
        &self,
        command: PromotePendingRunner,
    ) -> Result<PromotePendingRunnerOutcome, PromotePendingRunnerRepositoryError> {
        if command.command().as_uuid().is_nil() || command.command().as_uuid().is_max() {
            return Err(PromotePendingRunnerRepositoryError::InvalidCommandId);
        }
        let mut transaction = self.pool.begin().await?;
        match inspect_registry(&mut transaction, command.command()).await? {
            Some(CommandKind::PromotePendingRunner) => {
                let (recorded, result) = load_record(&mut transaction, command.command())
                    .await?
                    .ok_or(PromotePendingRunnerRepositoryError::Corruption(
                        "typed command record missing",
                    ))?;
                transaction.rollback().await?;
                return Ok(if recorded.pending_request() == command.pending_request() {
                    PromotePendingRunnerOutcome::Recorded(result)
                } else {
                    PromotePendingRunnerOutcome::ConflictingReuse {
                        command: command.command(),
                    }
                });
            }
            Some(_) => {
                transaction.rollback().await?;
                return Ok(PromotePendingRunnerOutcome::ConflictingReuse {
                    command: command.command(),
                });
            }
            None => {}
        }

        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at)
             VALUES ($1, $2, $3, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(durable_command_id_to_uuid(command.command()))
        .bind(PROMOTE_PENDING_RUNNER_KIND)
        .bind(STORAGE_VERSION)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            let outcome = resolve_claim_winner(&mut transaction, command).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }

        // This is the same temporary version-one deployment singleton lock
        // used by pristine enrollment admission. The command claim always
        // precedes it, and no session authority participates in promotion.
        sqlx::query("LOCK TABLE runner_enrollment IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *transaction)
            .await?;

        let pending = load_current_pending(&mut transaction, command.pending_request()).await?;
        let result = match pending {
            PendingSelection::None => PromotePendingRunnerResult::Rejected(
                PromotePendingRunnerRejection::NoPendingRunnerEnrollment,
            ),
            PendingSelection::Different => PromotePendingRunnerResult::Rejected(
                PromotePendingRunnerRejection::PendingRequestMismatch {
                    pending_request: command.pending_request(),
                },
            ),
            PendingSelection::Exact(pending) => {
                apply_or_reject_promotion(&mut transaction, command.pending_request(), pending)
                    .await?
            }
        };
        insert_record(&mut transaction, command, result).await?;
        transaction.commit().await.map_err(|error| {
            if crate::commit_failure_is_ambiguous(&error) {
                PromotePendingRunnerRepositoryError::CommitAmbiguous(error)
            } else {
                PromotePendingRunnerRepositoryError::Database(error)
            }
        })?;
        Ok(PromotePendingRunnerOutcome::Recorded(result))
    }
}

impl PromotePendingRunnerTransaction for PromotePendingRunnerRepository {
    type Error = PromotePendingRunnerRepositoryError;

    async fn handle(
        &mut self,
        command: PromotePendingRunner,
    ) -> Result<PromotePendingRunnerOutcome, Self::Error> {
        PromotePendingRunnerRepository::handle(self, command).await
    }
}

#[derive(Clone, Copy)]
struct PendingFacts {
    enrollment: RunnerEnrollmentId,
    predecessor: RunnerEnrollmentId,
}

enum PendingSelection {
    None,
    Different,
    Exact(PendingFacts),
}

#[derive(Clone)]
struct EnrollmentRow {
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    authentication: Uuid,
    allowed_class_count: Decimal,
    revision: u64,
    state: String,
}

async fn load_current_pending(
    connection: &mut PgConnection,
    request: RunnerEnrollmentRequestId,
) -> Result<PendingSelection, PromotePendingRunnerRepositoryError> {
    let exact = sqlx::query(
        "SELECT pending.enrollment_id, pending.predecessor_enrollment_id
           FROM runner_pending_enrollment AS pending
           JOIN runner_enrollment AS candidate
             ON candidate.enrollment_id = pending.enrollment_id
            AND candidate.state_kind = 'pending'
          WHERE pending.request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    if let Some(row) = exact {
        return Ok(PendingSelection::Exact(PendingFacts {
            enrollment: RunnerEnrollmentId::from_uuid(row.try_get("enrollment_id")?),
            predecessor: RunnerEnrollmentId::from_uuid(row.try_get("predecessor_enrollment_id")?),
        }));
    }
    let another: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM runner_pending_enrollment AS pending
              JOIN runner_enrollment AS candidate
                ON candidate.enrollment_id = pending.enrollment_id
               AND candidate.state_kind = 'pending'
        )",
    )
    .fetch_one(&mut *connection)
    .await?;
    Ok(if another {
        PendingSelection::Different
    } else {
        PendingSelection::None
    })
}

async fn apply_or_reject_promotion(
    connection: &mut PgConnection,
    request: RunnerEnrollmentRequestId,
    pending: PendingFacts,
) -> Result<PromotePendingRunnerResult, PromotePendingRunnerRepositoryError> {
    let enrollment_ids = [
        pending.enrollment.into_uuid(),
        pending.predecessor.into_uuid(),
    ];
    let rows = sqlx::query(PROMOTE_PENDING_RUNNER_ENROLLMENTS)
        .bind(enrollment_ids.as_slice())
        .fetch_all(&mut *connection)
        .await?;
    if rows.len() != 2 {
        return Err(PromotePendingRunnerRepositoryError::Corruption(
            "pending enrollment pair missing",
        ));
    }
    let mut decoded = rows
        .into_iter()
        .map(decode_enrollment_row)
        .collect::<Result<Vec<_>, _>>()?;
    let candidate_position = decoded
        .iter()
        .position(|row| row.enrollment == pending.enrollment)
        .ok_or(PromotePendingRunnerRepositoryError::Corruption(
            "pending candidate missing",
        ))?;
    let candidate = decoded.remove(candidate_position);
    let predecessor = decoded
        .pop()
        .ok_or(PromotePendingRunnerRepositoryError::Corruption(
            "active predecessor missing",
        ))?;
    if candidate.state != "pending"
        || candidate.revision != 1
        || predecessor.state != "active"
        || !matches!(predecessor.revision, 1 | 2)
    {
        return Err(PromotePendingRunnerRepositoryError::Corruption(
            "pending enrollment states",
        ));
    }

    let mut connection_order = [candidate.clone(), predecessor.clone()];
    connection_order.sort_by_key(|row| row.runner.into_uuid());
    let mut candidate_connection = None;
    let mut predecessor_connection = None;
    for enrollment in connection_order {
        let state = lock_connection_state(connection, enrollment.enrollment).await?;
        if enrollment.enrollment == candidate.enrollment {
            candidate_connection = state;
        } else {
            predecessor_connection = state;
        }
    }
    if candidate_connection != Some(RunnerConnectionState::Connected) {
        return Ok(PromotePendingRunnerResult::Rejected(
            PromotePendingRunnerRejection::PendingRequestDisconnected {
                pending_request: request,
            },
        ));
    }
    let predecessor_state = predecessor_connection.ok_or(
        PromotePendingRunnerRepositoryError::Corruption("predecessor connection head missing"),
    )?;
    let non_lost = match predecessor_state {
        RunnerConnectionState::Lost => None,
        RunnerConnectionState::Connected => Some(RunnerNonLostConnectionState::Connected),
        RunnerConnectionState::Suspect => Some(RunnerNonLostConnectionState::Suspect),
        RunnerConnectionState::Shutdown => Some(RunnerNonLostConnectionState::Shutdown),
    };
    if let Some(connection_state) = non_lost {
        return Ok(PromotePendingRunnerResult::Rejected(
            PromotePendingRunnerRejection::ActiveRunnerNotLost {
                runner: predecessor.runner,
                connection_state,
            },
        ));
    }

    let registration = sqlx::query(PROMOTE_PENDING_RUNNER_REGISTRATION)
        .bind(candidate.enrollment.into_uuid())
        .bind(request.into_uuid())
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(PromotePendingRunnerRepositoryError::Corruption(
            "pending registration head missing",
        ))?;
    let registration_revision = decode_u64(registration.try_get("registration_revision")?)?;
    let registration_runner = RunnerId::from_uuid(registration.try_get("runner_id")?);
    if registration_runner != candidate.runner {
        return Err(PromotePendingRunnerRepositoryError::Corruption(
            "pending registration runner",
        ));
    }

    let predecessor_revoked_revision = predecessor.revision.checked_add(1).ok_or(
        PromotePendingRunnerRepositoryError::Corruption("predecessor revision exhausted"),
    )?;
    append_enrollment_audit(
        connection,
        &predecessor,
        predecessor_revoked_revision,
        "revoked",
    )
    .await?;
    append_enrollment_audit(connection, &candidate, 2, "active").await?;
    let predecessor_updated = sqlx::query(
        "UPDATE runner_enrollment
            SET revision = $2, state_kind = 'revoked'
          WHERE enrollment_id = $1 AND revision = $3 AND state_kind = 'active'",
    )
    .bind(predecessor.enrollment.into_uuid())
    .bind(Decimal::from(predecessor_revoked_revision))
    .bind(Decimal::from(predecessor.revision))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    let candidate_updated = sqlx::query(
        "UPDATE runner_enrollment
            SET revision = 2, state_kind = 'active'
          WHERE enrollment_id = $1 AND revision = 1 AND state_kind = 'pending'",
    )
    .bind(candidate.enrollment.into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if predecessor_updated != 1 || candidate_updated != 1 {
        return Err(PromotePendingRunnerRepositoryError::Corruption(
            "enrollment promotion compare-and-swap",
        ));
    }
    let registration_revision = RunnerGeneration::try_from_u64(registration_revision).ok_or(
        PromotePendingRunnerRepositoryError::Corruption("registration revision encoding"),
    )?;
    Ok(PromotePendingRunnerResult::Applied(
        PromotedRunnerEnrollment::new(
            request,
            candidate.enrollment,
            candidate.runner,
            registration_revision,
        ),
    ))
}

fn decode_enrollment_row(row: PgRow) -> Result<EnrollmentRow, PromotePendingRunnerRepositoryError> {
    Ok(EnrollmentRow {
        enrollment: RunnerEnrollmentId::from_uuid(row.try_get("enrollment_id")?),
        runner: RunnerId::from_uuid(row.try_get("runner_id")?),
        authentication: row.try_get("authentication_reference_id")?,
        allowed_class_count: row.try_get("allowed_class_count")?,
        revision: decode_u64(row.try_get("revision")?)?,
        state: row.try_get("state_kind")?,
    })
}

async fn lock_connection_state(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
) -> Result<Option<RunnerConnectionState>, PromotePendingRunnerRepositoryError> {
    sqlx::query(PROMOTE_PENDING_RUNNER_CONNECTION)
        .bind(enrollment.into_uuid())
        .fetch_optional(&mut *connection)
        .await?
        .map(|row| {
            let state: String = row.try_get("state_kind")?;
            runner_connection_state_from_str(&state).ok_or(
                PromotePendingRunnerRepositoryError::Corruption("connection state discriminator"),
            )
        })
        .transpose()
}

async fn append_enrollment_audit(
    connection: &mut PgConnection,
    enrollment: &EnrollmentRow,
    revision: u64,
    state: &str,
) -> Result<(), PromotePendingRunnerRepositoryError> {
    sqlx::query(
        "INSERT INTO runner_enrollment_audit
            (enrollment_id, revision, runner_id, authentication_reference_id,
             allowed_class_count, state_kind)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(enrollment.enrollment.into_uuid())
    .bind(Decimal::from(revision))
    .bind(enrollment.runner.into_uuid())
    .bind(enrollment.authentication)
    .bind(enrollment.allowed_class_count)
    .bind(state)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO runner_enrollment_audit_allowed_class
            (enrollment_id, revision, capability_class)
         SELECT enrollment_id, $2, capability_class
           FROM runner_enrollment_allowed_class
          WHERE enrollment_id = $1",
    )
    .bind(enrollment.enrollment.into_uuid())
    .bind(Decimal::from(revision))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_record(
    connection: &mut PgConnection,
    command: PromotePendingRunner,
    result: PromotePendingRunnerResult,
) -> Result<(), PromotePendingRunnerRepositoryError> {
    let mut rejection = None;
    let mut result_enrollment = None;
    let mut result_runner = None;
    let mut result_revision = None;
    let mut active_enrollment: Option<Uuid> = None;
    let mut active_runner: Option<Uuid> = None;
    let mut active_state = None;
    let result_kind = match result {
        PromotePendingRunnerResult::Applied(applied) => {
            result_enrollment = Some(applied.enrollment().into_uuid());
            result_runner = Some(applied.runner().into_uuid());
            result_revision = Some(Decimal::from(applied.registration_revision().get()));
            PromotePendingRunnerResultStorageKind::Applied
        }
        PromotePendingRunnerResult::Rejected(reason) => {
            rejection = Some(promote_pending_runner_rejection_to_str(match reason {
                PromotePendingRunnerRejection::NoPendingRunnerEnrollment => {
                    PromotePendingRunnerRejectionStorageKind::NoPendingRunnerEnrollment
                }
                PromotePendingRunnerRejection::PendingRequestMismatch { .. } => {
                    PromotePendingRunnerRejectionStorageKind::PendingRequestMismatch
                }
                PromotePendingRunnerRejection::PendingRequestDisconnected { .. } => {
                    PromotePendingRunnerRejectionStorageKind::PendingRequestDisconnected
                }
                PromotePendingRunnerRejection::ActiveRunnerNotLost {
                    runner,
                    connection_state,
                } => {
                    active_runner = Some(runner.into_uuid());
                    active_state = Some(runner_non_lost_connection_state_to_str(connection_state));
                    PromotePendingRunnerRejectionStorageKind::ActiveRunnerNotLost
                }
            }));
            if matches!(
                reason,
                PromotePendingRunnerRejection::ActiveRunnerNotLost { .. }
            ) {
                active_enrollment = sqlx::query_scalar(
                    "SELECT predecessor_enrollment_id
                       FROM runner_pending_enrollment
                      WHERE request_id = $1",
                )
                .bind(command.pending_request().into_uuid())
                .fetch_optional(&mut *connection)
                .await?;
            }
            PromotePendingRunnerResultStorageKind::Rejected
        }
    };
    sqlx::query(
        "INSERT INTO promote_pending_runner_command
            (command_id, command_kind, storage_version, pending_request_id,
             result_kind, rejection_kind, result_enrollment_id,
             result_runner_id, result_registration_revision,
             active_enrollment_id, active_runner_id, active_connection_state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(durable_command_id_to_uuid(command.command()))
    .bind(PROMOTE_PENDING_RUNNER_KIND)
    .bind(STORAGE_VERSION)
    .bind(command.pending_request().into_uuid())
    .bind(promote_pending_runner_result_to_str(result_kind))
    .bind(rejection)
    .bind(result_enrollment)
    .bind(result_runner)
    .bind(result_revision)
    .bind(active_enrollment)
    .bind(active_runner)
    .bind(active_state)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn load_record(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<
    Option<(PromotePendingRunner, PromotePendingRunnerResult)>,
    PromotePendingRunnerRepositoryError,
> {
    let row = sqlx::query(
        "SELECT pending_request_id, result_kind, rejection_kind,
                result_enrollment_id, result_runner_id,
                result_registration_revision, active_runner_id,
                active_connection_state
           FROM promote_pending_runner_command
          WHERE command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;
    row.map(|row| decode_record(command_id, row)).transpose()
}

fn decode_record(
    command_id: DurableCommandId,
    row: PgRow,
) -> Result<(PromotePendingRunner, PromotePendingRunnerResult), PromotePendingRunnerRepositoryError>
{
    let request = RunnerEnrollmentRequestId::from_uuid(row.try_get("pending_request_id")?);
    let result_kind: String = row.try_get("result_kind")?;
    let result_kind = promote_pending_runner_result_from_str(&result_kind).ok_or(
        PromotePendingRunnerRepositoryError::Corruption("result spelling"),
    )?;
    let result = match result_kind {
        PromotePendingRunnerResultStorageKind::Applied => {
            let enrollment = RunnerEnrollmentId::from_uuid(required(&row, "result_enrollment_id")?);
            let runner = RunnerId::from_uuid(required(&row, "result_runner_id")?);
            let revision = RunnerGeneration::try_from_u64(decode_u64(required(
                &row,
                "result_registration_revision",
            )?)?)
            .ok_or(PromotePendingRunnerRepositoryError::Corruption(
                "registration revision encoding",
            ))?;
            PromotePendingRunnerResult::Applied(PromotedRunnerEnrollment::new(
                request, enrollment, runner, revision,
            ))
        }
        PromotePendingRunnerResultStorageKind::Rejected => {
            let rejection: String = required(&row, "rejection_kind")?;
            let rejection = promote_pending_runner_rejection_from_str(&rejection).ok_or(
                PromotePendingRunnerRepositoryError::Corruption("rejection spelling"),
            )?;
            let rejection = match rejection {
                PromotePendingRunnerRejectionStorageKind::NoPendingRunnerEnrollment => {
                    PromotePendingRunnerRejection::NoPendingRunnerEnrollment
                }
                PromotePendingRunnerRejectionStorageKind::PendingRequestMismatch => {
                    PromotePendingRunnerRejection::PendingRequestMismatch {
                        pending_request: request,
                    }
                }
                PromotePendingRunnerRejectionStorageKind::PendingRequestDisconnected => {
                    PromotePendingRunnerRejection::PendingRequestDisconnected {
                        pending_request: request,
                    }
                }
                PromotePendingRunnerRejectionStorageKind::ActiveRunnerNotLost => {
                    let runner = RunnerId::from_uuid(required(&row, "active_runner_id")?);
                    let state: String = required(&row, "active_connection_state")?;
                    PromotePendingRunnerRejection::ActiveRunnerNotLost {
                        runner,
                        connection_state: runner_non_lost_connection_state_from_str(&state).ok_or(
                            PromotePendingRunnerRepositoryError::Corruption(
                                "connection state spelling",
                            ),
                        )?,
                    }
                }
            };
            PromotePendingRunnerResult::Rejected(rejection)
        }
    };
    Ok((PromotePendingRunner::new(command_id, request), result))
}

async fn resolve_claim_winner(
    connection: &mut PgConnection,
    command: PromotePendingRunner,
) -> Result<PromotePendingRunnerOutcome, PromotePendingRunnerRepositoryError> {
    match inspect_registry(connection, command.command()).await? {
        Some(CommandKind::PromotePendingRunner) => {
            let (recorded, result) = load_record(connection, command.command()).await?.ok_or(
                PromotePendingRunnerRepositoryError::Corruption("winner typed record missing"),
            )?;
            Ok(if recorded.pending_request() == command.pending_request() {
                PromotePendingRunnerOutcome::Recorded(result)
            } else {
                PromotePendingRunnerOutcome::ConflictingReuse {
                    command: command.command(),
                }
            })
        }
        Some(_) => Ok(PromotePendingRunnerOutcome::ConflictingReuse {
            command: command.command(),
        }),
        None => Err(PromotePendingRunnerRepositoryError::Corruption(
            "winner command claim missing",
        )),
    }
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command: DurableCommandId,
) -> Result<Option<CommandKind>, PromotePendingRunnerRepositoryError> {
    command_registry::inspect(connection, command)
        .await
        .map_err(|error| match error {
            RegistryInspectionError::Database(error) => {
                PromotePendingRunnerRepositoryError::Database(error)
            }
            RegistryInspectionError::Corruption(_) => {
                PromotePendingRunnerRepositoryError::Corruption("durable command registry")
            }
        })
}

fn required<T>(row: &PgRow, column: &'static str) -> Result<T, PromotePendingRunnerRepositoryError>
where
    T: for<'row> sqlx::Decode<'row, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(column)?
        .ok_or(PromotePendingRunnerRepositoryError::Corruption(
            "required result field",
        ))
}

fn decode_u64(value: Decimal) -> Result<u64, PromotePendingRunnerRepositoryError> {
    value
        .to_u64()
        .filter(|decoded| Decimal::from(*decoded) == value)
        .ok_or(PromotePendingRunnerRepositoryError::Corruption(
            "positive integer encoding",
        ))
}
