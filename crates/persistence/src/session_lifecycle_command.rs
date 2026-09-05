//! Claim, apply, and settle the session-lifecycle command family
//! (docs/spec/session-lifecycle.md).
//!
//! One registry kind carries the lifecycle operations. Every claimed command
//! records its typed row and a `command_settled` receipt; a closure that finds
//! a live turn commits its outcome to the satellite's handoff and reports the
//! turn the committed interrupt machinery settles.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    CommandPrincipal, DescendantTerminationScope, DurableCommandId, GoalState, LifecycleActor,
    SessionConfigurationDefaultsVersion, SessionFailureCause, SessionId,
    SessionLifecycleApplication, SessionLifecycleCommand, SessionLifecycleCommandRejection,
    SessionLifecycleCommandResult, SessionLifecycleOperation, SessionLifecycleState,
    SessionTerminalOutcome, StopStickiness, TurnId,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{self, CommandKind, RegistryInspectionError, SESSION_LIFECYCLE_KIND},
    mapping::{
        defaults_version_from_numeric, durable_command_id_to_uuid, finish_condition_columns,
        finish_condition_from_columns, session_id_from_uuid, session_id_to_uuid,
        session_lifecycle_command_rejection_from_str, session_lifecycle_command_rejection_to_str,
        session_lifecycle_operation_to_str, session_retryable_cause_from_str,
        session_retryable_cause_to_str, session_structural_cause_from_str,
        session_structural_cause_to_str, turn_id_from_uuid, turn_id_to_uuid,
    },
    outbox::{self, CommandSettlementOutbox, OutboxEvent},
    session_lifecycle::{
        self, SessionLifecycleRejection, SessionLifecycleRepositoryError, load_locked,
    },
};

const STORAGE_VERSION: i16 = 1;

/// The committed outcome of handling one lifecycle command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycleCommandHandlingOutcome {
    /// First handling or equal replay returns the recorded result.
    Recorded(SessionLifecycleCommandResult),
    /// The identifier is already bound to a structurally different payload.
    ConflictingReuse {
        /// The user-global identifier whose earlier meaning is retained.
        command_id: DurableCommandId,
    },
}

/// A database failure, ambiguous commit, or durable shape that cannot
/// construct the domain value.
#[derive(Debug)]
pub enum SessionLifecycleCommandRepositoryError {
    /// The database rejected or could not run one statement.
    Database(sqlx::Error),
    /// The commit response was lost; the outcome is unknown.
    CommitAmbiguous(sqlx::Error),
    /// Durable state cannot construct the domain value.
    Corruption(&'static str),
    /// The lifecycle satellite failed beneath the command.
    Lifecycle(Box<SessionLifecycleRepositoryError>),
}

impl fmt::Display for SessionLifecycleCommandRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(_) => formatter.write_str("session lifecycle command database failure"),
            Self::CommitAmbiguous(_) => {
                formatter.write_str("session lifecycle command commit outcome is unknown")
            }
            Self::Corruption(detail) => {
                write!(formatter, "session lifecycle command is corrupt: {detail}")
            }
            Self::Lifecycle(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for SessionLifecycleCommandRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Lifecycle(error) => Some(error.as_ref()),
            Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for SessionLifecycleCommandRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<RegistryInspectionError> for SessionLifecycleCommandRepositoryError {
    fn from(error: RegistryInspectionError) -> Self {
        match error {
            RegistryInspectionError::Database(error) => Self::Database(error),
            RegistryInspectionError::Corruption(_) => Self::Corruption("command registry"),
        }
    }
}

/// PostgreSQL implementation of the lifecycle command port.
#[derive(Clone, Debug)]
pub struct SessionLifecycleCommandRepository {
    pool: PgPool,
}

impl SessionLifecycleCommandRepository {
    /// Uses the supplied pool for atomic handling and replay.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claims and applies an unseen command, or resolves its durable meaning.
    pub async fn handle(
        &self,
        command: SessionLifecycleCommand,
        principal: CommandPrincipal,
    ) -> Result<SessionLifecycleCommandHandlingOutcome, SessionLifecycleCommandRepositoryError>
    {
        let command_id = command.command_id();
        let mut transaction = self.pool.begin().await?;
        if let Some(kind) = command_registry::inspect(&mut transaction, command_id).await? {
            let outcome = existing_or_conflicting(&mut transaction, &command, kind).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }
        let issuer = command_registry::issuer_columns(principal);
        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at,
                 issuer_kind, issuer_module)
             VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
             ON CONFLICT DO NOTHING",
        )
        .bind(durable_command_id_to_uuid(command_id))
        .bind(SESSION_LIFECYCLE_KIND)
        .bind(STORAGE_VERSION)
        .bind(issuer.0)
        .bind(issuer.1)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            let kind = command_registry::inspect(&mut transaction, command_id)
                .await?
                .ok_or(SessionLifecycleCommandRepositoryError::Corruption(
                    "winner command claim disappeared",
                ))?;
            let outcome = existing_or_conflicting(&mut transaction, &command, kind).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let actor = principal.classify(None);
        sqlx::query("SAVEPOINT lifecycle_apply")
            .execute(&mut *transaction)
            .await?;
        let result = match apply(&mut transaction, &command, actor).await {
            Ok(result) => SessionLifecycleCommandResult::Applied(result),
            Err(ApplyError::Rejected(rejection)) => {
                sqlx::query("ROLLBACK TO SAVEPOINT lifecycle_apply")
                    .execute(&mut *transaction)
                    .await?;
                SessionLifecycleCommandResult::Rejected(rejection)
            }
            Err(ApplyError::Failed(error)) => {
                transaction.rollback().await?;
                return Err(error);
            }
        };
        sqlx::query("RELEASE SAVEPOINT lifecycle_apply")
            .execute(&mut *transaction)
            .await?;
        insert_command_record(&mut transaction, &command, result).await?;
        if matches!(
            (command.operation(), result),
            (
                SessionLifecycleOperation::Stop {
                    descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    ..
                },
                SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::Closed { .. })
            )
        ) {
            sqlx::query("SELECT materialize_session_delegation_termination_cascade($1, 'stopped')")
                .bind(durable_command_id_to_uuid(command.command_id()))
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await.map_err(|error| {
            if crate::commit_failure_is_ambiguous(&error) {
                SessionLifecycleCommandRepositoryError::CommitAmbiguous(error)
            } else {
                SessionLifecycleCommandRepositoryError::Database(error)
            }
        })?;
        Ok(SessionLifecycleCommandHandlingOutcome::Recorded(result))
    }
}

enum ApplyError {
    Rejected(SessionLifecycleCommandRejection),
    Failed(SessionLifecycleCommandRepositoryError),
}

impl From<sqlx::Error> for ApplyError {
    fn from(error: sqlx::Error) -> Self {
        Self::Failed(error.into())
    }
}

impl From<SessionLifecycleRepositoryError> for ApplyError {
    fn from(error: SessionLifecycleRepositoryError) -> Self {
        match error {
            SessionLifecycleRepositoryError::Rejected(rejection) => {
                Self::Rejected(command_rejection(rejection))
            }
            SessionLifecycleRepositoryError::UnknownSession(_) => {
                Self::Rejected(SessionLifecycleCommandRejection::SessionNotFound)
            }
            SessionLifecycleRepositoryError::Database(error) => {
                Self::Failed(SessionLifecycleCommandRepositoryError::Database(error))
            }
            SessionLifecycleRepositoryError::CommitAmbiguous(error) => Self::Failed(
                SessionLifecycleCommandRepositoryError::CommitAmbiguous(error),
            ),
            other => Self::Failed(SessionLifecycleCommandRepositoryError::Lifecycle(Box::new(
                other,
            ))),
        }
    }
}

const fn command_rejection(
    rejection: SessionLifecycleRejection,
) -> SessionLifecycleCommandRejection {
    match rejection {
        SessionLifecycleRejection::TransitionNotAdmitted
        | SessionLifecycleRejection::GoalGenerationStillOpen
        | SessionLifecycleRejection::NoPendingTerminal
        | SessionLifecycleRejection::ParkWhileUnmonitored
        | SessionLifecycleRejection::AbandonRequiresParkedOperator => {
            SessionLifecycleCommandRejection::TransitionNotAdmitted
        }
        SessionLifecycleRejection::ReleaseWhileParked => {
            SessionLifecycleCommandRejection::ReleaseWhileParked
        }
        SessionLifecycleRejection::OwnershipUnchanged => {
            SessionLifecycleCommandRejection::OwnershipUnchanged
        }
        SessionLifecycleRejection::PendingTerminalConflict => {
            SessionLifecycleCommandRejection::PendingTerminalConflict
        }
        SessionLifecycleRejection::GoalOutcomeMismatch => {
            SessionLifecycleCommandRejection::GoalOutcomeMismatch
        }
        SessionLifecycleRejection::StandingCauseMismatch
        | SessionLifecycleRejection::ParkStandingMismatch => {
            SessionLifecycleCommandRejection::StandingCauseMismatch
        }
        SessionLifecycleRejection::FinishConditionAlreadyDeclared => {
            SessionLifecycleCommandRejection::FinishConditionAlreadyDeclared
        }
    }
}

async fn apply(
    connection: &mut PgConnection,
    command: &SessionLifecycleCommand,
    actor: LifecycleActor,
) -> Result<SessionLifecycleApplication, ApplyError> {
    let session = command.session();
    if matches!(
        command.operation(),
        SessionLifecycleOperation::Stop {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        }
    ) {
        sqlx::query(crate::lock_inventory::DELEGATION_TERMINATION_SESSION_FRONTIER)
            .bind(session_id_to_uuid(session))
            .bind("stopped")
            .execute(&mut *connection)
            .await?;
    }
    let held = match load_locked(connection, session).await {
        Ok(held) => held,
        Err(SessionLifecycleRepositoryError::UnknownSession(_)) => {
            return Err(ApplyError::Rejected(
                SessionLifecycleCommandRejection::SessionNotFound,
            ));
        }
        Err(error) => return Err(error.into()),
    };
    match command.operation() {
        SessionLifecycleOperation::ReleaseStart => {
            session_lifecycle::release_start_in_transaction(connection, session, actor).await?;
            Ok(SessionLifecycleApplication::StartReleased)
        }
        SessionLifecycleOperation::Stop { sticky, .. } => {
            close(
                connection,
                session,
                SessionTerminalOutcome::Stopped { sticky: *sticky },
                actor,
            )
            .await
        }
        SessionLifecycleOperation::Supersede { successor } => {
            if *successor == session {
                return Err(ApplyError::Rejected(
                    SessionLifecycleCommandRejection::SuccessorIsSelf,
                ));
            }
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM session WHERE session_id = $1)")
                    .bind(session_id_to_uuid(*successor))
                    .fetch_one(&mut *connection)
                    .await?;
            if !exists {
                return Err(ApplyError::Rejected(
                    SessionLifecycleCommandRejection::SuccessorNotFound,
                ));
            }
            close(
                connection,
                session,
                SessionTerminalOutcome::Superseded {
                    by: Some(*successor),
                },
                actor,
            )
            .await
        }
        SessionLifecycleOperation::Abandon => {
            require_parked(&held.state())?;
            close(
                connection,
                session,
                SessionTerminalOutcome::Abandoned,
                actor,
            )
            .await
        }
        SessionLifecycleOperation::CloseFailed { cause } => {
            let standing = require_parked(&held.state())?;
            if cause.is_some() && *cause != standing {
                return Err(ApplyError::Rejected(
                    SessionLifecycleCommandRejection::StandingCauseMismatch,
                ));
            }
            let outcome = match cause.or(standing) {
                Some(SessionFailureCause::Retryable(cause)) => {
                    SessionTerminalOutcome::FailedRetryable { cause }
                }
                Some(SessionFailureCause::Structural(cause)) => {
                    SessionTerminalOutcome::FailedStructural { cause }
                }
                None => SessionTerminalOutcome::FailedUnknown,
            };
            close(connection, session, outcome, actor).await
        }
        SessionLifecycleOperation::Resume => {
            require_parked(&held.state())?;
            let goal = crate::goal::load_goal_from_connection(connection, session)
                .await
                .map_err(|error| match error {
                    crate::goal::GoalRepositoryError::Database(error)
                    | crate::goal::GoalRepositoryError::CommitAmbiguous(error) => {
                        ApplyError::Failed(SessionLifecycleCommandRepositoryError::Database(error))
                    }
                    _ => ApplyError::Failed(SessionLifecycleCommandRepositoryError::Corruption(
                        "goal lineage",
                    )),
                })?;
            if goal
                .as_ref()
                .is_some_and(|goal| matches!(goal.current().state(), GoalState::Blocked { .. }))
            {
                return Err(ApplyError::Rejected(
                    SessionLifecycleCommandRejection::GoalResumeRequired,
                ));
            }
            let state =
                session_lifecycle::resume_in_transaction(connection, session, actor).await?;
            Ok(SessionLifecycleApplication::Resumed { state })
        }
        SessionLifecycleOperation::Adopt { finish_condition } => {
            session_lifecycle::adopt_in_transaction(
                connection,
                session,
                finish_condition.clone(),
                actor,
            )
            .await?;
            Ok(SessionLifecycleApplication::OwnershipChanged)
        }
        SessionLifecycleOperation::Release => {
            session_lifecycle::release_in_transaction(connection, session, actor).await?;
            Ok(SessionLifecycleApplication::OwnershipChanged)
        }
    }
}

fn require_parked(
    state: &SessionLifecycleState,
) -> Result<Option<SessionFailureCause>, ApplyError> {
    match state {
        SessionLifecycleState::Parked { standing, .. } => Ok(*standing),
        _ => Err(ApplyError::Rejected(
            SessionLifecycleCommandRejection::RequiresParked,
        )),
    }
}

/// Closes now when no turn is live, or commits the outcome to the handoff and
/// names the turn the interrupt machinery settles.
async fn close(
    connection: &mut PgConnection,
    session: SessionId,
    outcome: SessionTerminalOutcome,
    actor: LifecycleActor,
) -> Result<SessionLifecycleApplication, ApplyError> {
    let live_turn = live_active_turn(connection, session).await?;
    match live_turn {
        Some(live_turn) => {
            session_lifecycle::commit_pending_terminal_in_transaction(
                connection, session, outcome, actor,
            )
            .await?;
            let defaults_version = current_defaults_version(connection, session).await?;
            Ok(SessionLifecycleApplication::ClosurePending {
                outcome,
                live_turn,
                defaults_version,
            })
        }
        None => {
            retire_queued_turns(connection, session).await?;
            session_lifecycle::close_in_transaction(connection, session, outcome, actor).await?;
            Ok(SessionLifecycleApplication::Closed { outcome })
        }
    }
}

/// The defaults epoch a closure's interrupt names: the one current at the
/// lifecycle transaction, first handling or replay.
async fn current_defaults_version(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<SessionConfigurationDefaultsVersion, ApplyError> {
    let version: Option<Decimal> = sqlx::query_scalar(
        "SELECT current_version FROM session_current_defaults WHERE session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    version
        .and_then(|version| defaults_version_from_numeric(version).ok())
        .ok_or(ApplyError::Failed(
            SessionLifecycleCommandRepositoryError::Corruption("session defaults version"),
        ))
}

async fn live_active_turn(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<TurnId>, sqlx::Error> {
    let turn: Option<Uuid> = sqlx::query_scalar(
        "SELECT turn_id FROM turn_lifecycle
          WHERE session_id = $1
            AND state_kind = 'active'
            AND NOT delegation_runtime_terminal
          ORDER BY acceptance_position DESC
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    Ok(turn.map(turn_id_from_uuid))
}

/// Retires every queued turn the closure strands, one at a time so
/// each retirement publishes its own `turn_terminal{retired}`.
pub(crate) async fn retire_queued_turns(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), sqlx::Error> {
    loop {
        let retired: Option<Uuid> = sqlx::query_scalar(
            "UPDATE turn_lifecycle
                SET state_kind = 'terminal',
                    terminal_disposition_kind = 'retired',
                    terminal_cause_kind = $2
              WHERE session_id = $1
                AND turn_id = (
                    SELECT turn_id FROM turn_lifecycle
                     WHERE session_id = $1 AND state_kind = 'queued'
                     ORDER BY acceptance_position
                     LIMIT 1
                )
            RETURNING turn_id",
        )
        .bind(session_id_to_uuid(session))
        .bind(crate::mapping::turn_terminal_cause_to_str(
            signalbox_domain::TurnTerminalCause::SessionClosed,
        ))
        .fetch_optional(&mut *connection)
        .await?;
        let Some(turn) = retired else {
            return Ok(());
        };
        outbox::append(
            connection,
            OutboxEvent::TurnTerminal {
                session,
                turn: turn_id_from_uuid(turn),
                disposition: outbox::TurnTerminalOutboxDisposition::Retired,
            },
        )
        .await?;
    }
}

async fn insert_command_record(
    connection: &mut PgConnection,
    command: &SessionLifecycleCommand,
    result: SessionLifecycleCommandResult,
) -> Result<(), sqlx::Error> {
    let operation = command.operation();
    let (sticky, scope) = match operation {
        SessionLifecycleOperation::Stop {
            sticky,
            descendant_scope,
        } => (
            Some(matches!(sticky, StopStickiness::Sticky)),
            Some(descendant_scope_to_str(*descendant_scope)),
        ),
        _ => (None, None),
    };
    let successor = match operation {
        SessionLifecycleOperation::Supersede { successor } => Some(session_id_to_uuid(*successor)),
        _ => None,
    };
    let failure_cause = match operation {
        SessionLifecycleOperation::CloseFailed {
            cause: Some(SessionFailureCause::Retryable(cause)),
        } => Some(session_retryable_cause_to_str(*cause)),
        SessionLifecycleOperation::CloseFailed {
            cause: Some(SessionFailureCause::Structural(cause)),
        } => Some(session_structural_cause_to_str(*cause)),
        _ => None,
    };
    let (finish_kind, finish_statement) = match operation {
        SessionLifecycleOperation::Adopt { finish_condition } => {
            finish_condition_columns(finish_condition.as_ref())
        }
        _ => (None, None),
    };
    let (result_kind, rejection) = match result {
        SessionLifecycleCommandResult::Applied(_) => ("applied", None),
        SessionLifecycleCommandResult::Rejected(rejection) => (
            "rejected",
            Some(session_lifecycle_command_rejection_to_str(rejection)),
        ),
    };
    let (effect, live_turn) = match result {
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::StartReleased) => {
            (Some("start_released"), None)
        }
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::Closed { .. }) => {
            (Some("closed"), None)
        }
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::ClosurePending {
            live_turn,
            ..
        }) => (Some("closure_pending"), Some(turn_id_to_uuid(live_turn))),
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::Resumed { .. }) => {
            (Some("resumed"), None)
        }
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::OwnershipChanged) => {
            (Some("ownership_changed"), None)
        }
        SessionLifecycleCommandResult::Rejected(_) => (None, None),
    };
    sqlx::query(
        "INSERT INTO session_lifecycle_command
            (command_id, command_kind, storage_version, session_id, operation_kind,
             stop_sticky, descendant_scope, successor_session_id, failure_cause_kind,
             finish_condition_kind, finish_condition, result_kind, rejection_kind,
             applied_effect_kind, live_turn_id, resume_state_kind,
             resume_waiting_kind, resume_waiting_waker,
             resume_waiting_subject_session_id, resume_recovering_op,
             resume_blocked_reason, resume_blocked_cycle)
         SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                CASE WHEN $14::text = 'resumed' THEN lifecycle.state_kind END,
                CASE WHEN $14::text = 'resumed' THEN lifecycle.waiting_kind END,
                CASE WHEN $14::text = 'resumed' THEN lifecycle.waiting_waker END,
                CASE WHEN $14::text = 'resumed'
                    THEN lifecycle.waiting_subject_session_id END,
                CASE WHEN $14::text = 'resumed' THEN lifecycle.recovering_op END,
                CASE WHEN $14::text = 'resumed' THEN lifecycle.blocked_reason END,
                CASE WHEN $14::text = 'resumed' THEN lifecycle.blocked_cycle END
           FROM (VALUES (true)) AS singleton(present)
           LEFT JOIN session_lifecycle AS lifecycle ON lifecycle.session_id = $4",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(SESSION_LIFECYCLE_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(session_lifecycle_operation_to_str(operation))
    .bind(sticky)
    .bind(scope)
    .bind(successor)
    .bind(failure_cause)
    .bind(finish_kind)
    .bind(finish_statement)
    .bind(result_kind)
    .bind(rejection)
    .bind(effect)
    .bind(live_turn)
    .execute(&mut *connection)
    .await?;
    // A rejection naming a session that does not exist settles without one.
    let settled_session = match result {
        SessionLifecycleCommandResult::Rejected(
            SessionLifecycleCommandRejection::SessionNotFound,
        ) => None,
        _ => Some(command.session()),
    };
    outbox::append(
        connection,
        OutboxEvent::CommandSettled {
            session: settled_session,
            command: command.command_id(),
            result: match rejection {
                None => CommandSettlementOutbox::Applied,
                Some(kind) => CommandSettlementOutbox::Rejected { kind },
            },
        },
    )
    .await
}

async fn existing_or_conflicting(
    connection: &mut PgConnection,
    command: &SessionLifecycleCommand,
    kind: CommandKind,
) -> Result<SessionLifecycleCommandHandlingOutcome, SessionLifecycleCommandRepositoryError> {
    let conflicting = SessionLifecycleCommandHandlingOutcome::ConflictingReuse {
        command_id: command.command_id(),
    };
    if kind != CommandKind::SessionLifecycle {
        return Ok(conflicting);
    }
    let Some((recorded, result)) = load_recorded(connection, command.command_id()).await? else {
        return Err(SessionLifecycleCommandRepositoryError::Corruption(
            "claimed lifecycle command has no typed record",
        ));
    };
    if recorded != *command {
        return Ok(conflicting);
    }
    let result = match result {
        RecordedResult::Rejected(rejection) => SessionLifecycleCommandResult::Rejected(rejection),
        RecordedResult::Applied(effect) => SessionLifecycleCommandResult::Applied(
            replayed_application(connection, &recorded, effect).await?,
        ),
    };
    Ok(SessionLifecycleCommandHandlingOutcome::Recorded(result))
}

/// Replays the recorded effect. Closure outcomes are fixed in the satellite;
/// a resume carries its mapped state in the command record because that state
/// can move again.
async fn replayed_application(
    connection: &mut PgConnection,
    command: &SessionLifecycleCommand,
    effect: RecordedEffect,
) -> Result<SessionLifecycleApplication, SessionLifecycleCommandRepositoryError> {
    let record = session_lifecycle::load_optional(connection, command.session())
        .await
        .map_err(|error| SessionLifecycleCommandRepositoryError::Lifecycle(Box::new(error)))?
        .ok_or(SessionLifecycleCommandRepositoryError::Corruption(
            "applied lifecycle command names no session",
        ))?;
    let outcome = || match (record.state(), record.pending_terminal()) {
        (SessionLifecycleState::Terminal { outcome }, _) | (_, Some(outcome)) => Ok(outcome),
        (_, None) => Err(SessionLifecycleCommandRepositoryError::Corruption(
            "applied closure left no outcome",
        )),
    };
    Ok(match effect {
        RecordedEffect::StartReleased => SessionLifecycleApplication::StartReleased,
        RecordedEffect::Closed => SessionLifecycleApplication::Closed {
            outcome: outcome()?,
        },
        RecordedEffect::ClosurePending { live_turn } => {
            SessionLifecycleApplication::ClosurePending {
                outcome: outcome()?,
                live_turn,
                defaults_version: current_defaults_version(connection, command.session())
                    .await
                    .map_err(|error| match error {
                        ApplyError::Failed(error) => error,
                        ApplyError::Rejected(_) => {
                            SessionLifecycleCommandRepositoryError::Corruption(
                                "replayed closure lost its defaults epoch",
                            )
                        }
                    })?,
            }
        }
        RecordedEffect::Resumed { state } => SessionLifecycleApplication::Resumed { state },
        RecordedEffect::OwnershipChanged => SessionLifecycleApplication::OwnershipChanged,
    })
}

enum RecordedEffect {
    StartReleased,
    Closed,
    ClosurePending { live_turn: TurnId },
    Resumed { state: SessionLifecycleState },
    OwnershipChanged,
}

enum RecordedResult {
    Applied(RecordedEffect),
    Rejected(SessionLifecycleCommandRejection),
}

async fn load_recorded(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<(SessionLifecycleCommand, RecordedResult)>, SessionLifecycleCommandRepositoryError>
{
    let row = sqlx::query(
        "SELECT session_id, operation_kind, stop_sticky, descendant_scope,
                successor_session_id, failure_cause_kind, finish_condition_kind,
                finish_condition, result_kind, rejection_kind, applied_effect_kind,
                live_turn_id, resume_state_kind AS state_kind,
                resume_waiting_kind AS waiting_kind,
                resume_waiting_waker AS waiting_waker,
                resume_waiting_subject_session_id AS waiting_subject_session_id,
                resume_recovering_op AS recovering_op,
                resume_blocked_reason AS blocked_reason,
                resume_blocked_cycle AS blocked_cycle,
                NULL::text AS parked_cause,
                NULL::text AS parked_responder,
                NULL::text AS parked_standing_cause_kind,
                NULL::text AS terminal_outcome_kind,
                NULL::text AS terminal_cause_kind,
                NULL::boolean AS terminal_stop_sticky,
                NULL::uuid AS terminal_superseded_by
           FROM session_lifecycle_command
          WHERE command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;
    row.map(|row| decode_recorded(&row, command_id)).transpose()
}

fn decode_recorded(
    row: &PgRow,
    command_id: DurableCommandId,
) -> Result<(SessionLifecycleCommand, RecordedResult), SessionLifecycleCommandRepositoryError> {
    let corrupt = SessionLifecycleCommandRepositoryError::Corruption;
    let session = session_id_from_uuid(row.try_get("session_id")?);
    let operation: String = row.try_get("operation_kind")?;
    let sticky: Option<bool> = row.try_get("stop_sticky")?;
    let scope: Option<String> = row.try_get("descendant_scope")?;
    let successor: Option<Uuid> = row.try_get("successor_session_id")?;
    let failure_cause: Option<String> = row.try_get("failure_cause_kind")?;
    let finish_condition = finish_condition_from_columns(
        row.try_get("finish_condition_kind")?,
        row.try_get("finish_condition")?,
    )
    .map_err(corrupt)?;
    let operation = match (operation.as_str(), sticky, scope, successor, failure_cause) {
        ("release_start", None, None, None, None) => SessionLifecycleOperation::ReleaseStart,
        ("stop", Some(sticky), Some(scope), None, None) => SessionLifecycleOperation::Stop {
            sticky: if sticky {
                StopStickiness::Sticky
            } else {
                StopStickiness::Redispatchable
            },
            descendant_scope: descendant_scope_from_str(&scope)
                .ok_or(corrupt("descendant scope"))?,
        },
        ("supersede", None, None, Some(successor), None) => SessionLifecycleOperation::Supersede {
            successor: session_id_from_uuid(successor),
        },
        ("abandon", None, None, None, None) => SessionLifecycleOperation::Abandon,
        ("close_failed", None, None, None, cause) => SessionLifecycleOperation::CloseFailed {
            cause: cause
                .map(|cause| decode_failure_cause(&cause).ok_or(corrupt("failure cause")))
                .transpose()?,
        },
        ("resume", None, None, None, None) => SessionLifecycleOperation::Resume,
        ("adopt", None, None, None, None) => SessionLifecycleOperation::Adopt { finish_condition },
        ("release", None, None, None, None) => SessionLifecycleOperation::Release,
        _ => return Err(corrupt("operation shape")),
    };
    if !matches!(operation, SessionLifecycleOperation::Adopt { .. })
        && finish_condition_present(row)?
    {
        return Err(corrupt("finish condition on a non-adopt"));
    }
    let result_kind: String = row.try_get("result_kind")?;
    let rejection: Option<String> = row.try_get("rejection_kind")?;
    let effect: Option<String> = row.try_get("applied_effect_kind")?;
    let live_turn: Option<Uuid> = row.try_get("live_turn_id")?;
    let result = match (
        result_kind.as_str(),
        rejection,
        effect.as_deref(),
        live_turn,
    ) {
        ("applied", None, Some("start_released"), None) => {
            RecordedResult::Applied(RecordedEffect::StartReleased)
        }
        ("applied", None, Some("closed"), None) => RecordedResult::Applied(RecordedEffect::Closed),
        ("applied", None, Some("closure_pending"), Some(live_turn)) => {
            RecordedResult::Applied(RecordedEffect::ClosurePending {
                live_turn: turn_id_from_uuid(live_turn),
            })
        }
        ("applied", None, Some("resumed"), None) => {
            RecordedResult::Applied(RecordedEffect::Resumed {
                state: session_lifecycle::decode_lifecycle_state(row)
                    .map_err(|_| corrupt("resume state"))?,
            })
        }
        ("applied", None, Some("ownership_changed"), None) => {
            RecordedResult::Applied(RecordedEffect::OwnershipChanged)
        }
        ("rejected", Some(rejection), None, None) => RecordedResult::Rejected(
            session_lifecycle_command_rejection_from_str(&rejection)
                .ok_or(corrupt("rejection kind"))?,
        ),
        _ => return Err(corrupt("result shape")),
    };
    Ok((
        SessionLifecycleCommand::new(command_id, session, operation),
        result,
    ))
}

const fn descendant_scope_to_str(value: DescendantTerminationScope) -> &'static str {
    match value {
        DescendantTerminationScope::ParentAlone => "parent_alone",
        DescendantTerminationScope::ParentAndDescendants => "parent_and_descendants",
    }
}

fn descendant_scope_from_str(value: &str) -> Option<DescendantTerminationScope> {
    match value {
        "parent_alone" => Some(DescendantTerminationScope::ParentAlone),
        "parent_and_descendants" => Some(DescendantTerminationScope::ParentAndDescendants),
        _ => None,
    }
}

fn finish_condition_present(row: &PgRow) -> Result<bool, sqlx::Error> {
    Ok(row
        .try_get::<Option<String>, _>("finish_condition_kind")?
        .is_some())
}

fn decode_failure_cause(cause: &str) -> Option<SessionFailureCause> {
    session_retryable_cause_from_str(cause)
        .map(SessionFailureCause::Retryable)
        .or_else(|| session_structural_cause_from_str(cause).map(SessionFailureCause::Structural))
}
