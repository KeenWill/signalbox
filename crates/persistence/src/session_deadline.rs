//! Transactional expiry of core-owned session admission and waiting deadlines.

use std::{error::Error, fmt, time::Duration};

use signalbox_domain::{
    LifecycleActor, SessionId, SessionLifecycleState, SessionParkCause, SessionParkResponder,
    SessionRetirementCause, SessionTerminalOutcome,
};
use sqlx::{PgPool, Row, types::Uuid};

use crate::{
    mapping::session_id_from_uuid,
    session_lifecycle::{self, SessionLifecycleRepositoryError},
    session_lifecycle_command::retire_queued_turns,
};

/// Configured bounds consumed by the core expiry pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionDeadlineBounds {
    admission: Option<Duration>,
    waiting: Option<Duration>,
}

impl SessionDeadlineBounds {
    /// Uses the already-validated lifecycle deadline configuration.
    pub const fn new(admission: Option<Duration>, waiting: Option<Duration>) -> Self {
        Self { admission, waiting }
    }
}

/// What one oldest-due pass changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDeadlinePassOutcome {
    /// No supported deadline is currently due.
    Idle,
    /// One supported deadline materialized its configured expiry.
    Armed { session: SessionId },
    /// Admission expired after turn activity, so its deadline was settled.
    Superseded { session: SessionId },
    /// Admission expired and the session retired.
    Retired { session: SessionId },
    /// A waiting deadline expired and the live turn was suspended in place.
    Parked { session: SessionId },
}

/// Failure to materialize or apply one deadline.
#[derive(Debug)]
pub enum SessionDeadlineRepositoryError {
    /// A configured duration cannot fit the storage arithmetic.
    BoundExceedsStorage,
    /// PostgreSQL rejected or could not run one statement.
    Database {
        /// Statement or transaction operation that failed.
        query: &'static str,
        /// Original PostgreSQL or pool failure.
        source: sqlx::Error,
    },
    /// The lifecycle transition failed beneath the pass.
    Lifecycle {
        /// Lifecycle operation that failed.
        query: &'static str,
        /// Original lifecycle failure.
        source: Box<SessionLifecycleRepositoryError>,
    },
}

impl fmt::Display for SessionDeadlineRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BoundExceedsStorage => {
                formatter.write_str("session deadline bound exceeds storage")
            }
            Self::Database { query, .. } => {
                write!(formatter, "session deadline database failure in {query}")
            }
            Self::Lifecycle { query, source } => write!(formatter, "{query}: {source}"),
        }
    }
}

impl Error for SessionDeadlineRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Lifecycle { source, .. } => Some(source.as_ref()),
            Self::BoundExceedsStorage => None,
        }
    }
}

/// PostgreSQL implementation of the core expiry pass.
#[derive(Clone, Debug)]
pub struct PostgresSessionDeadlineRepository {
    pool: PgPool,
    bounds: SessionDeadlineBounds,
}

impl PostgresSessionDeadlineRepository {
    /// Uses the supplied pool and configured admission/waiting bounds.
    pub const fn new(pool: PgPool, bounds: SessionDeadlineBounds) -> Self {
        Self { pool, bounds }
    }

    /// Applies the oldest currently due supported deadline, if one exists.
    pub async fn expire_next(
        &self,
    ) -> Result<SessionDeadlinePassOutcome, SessionDeadlineRepositoryError> {
        let admission_millis = stored_millis(self.bounds.admission)?;
        let waiting_millis = stored_millis(self.bounds.waiting)?;
        let candidate: Option<Uuid> = sqlx::query_scalar(
            "SELECT session_id
               FROM session_deadline
              WHERE NOT settled
                AND ((deadline_kind = 'admission'
                     AND (expires_at IS DISTINCT FROM CASE
                              WHEN $1::BIGINT IS NULL THEN NULL
                              ELSE armed_at + $1 * INTERVAL '1 millisecond'
                          END
                          OR expires_at <= clock_timestamp()))
                 OR (deadline_kind = 'waiting'
                     AND (expires_at IS DISTINCT FROM CASE
                              WHEN $2::BIGINT IS NULL THEN NULL
                              ELSE armed_at + $2 * INTERVAL '1 millisecond'
                          END
                          OR expires_at <= clock_timestamp())))
              ORDER BY COALESCE(
                           expires_at,
                           CASE deadline_kind
                               WHEN 'admission' THEN armed_at + $1 * INTERVAL '1 millisecond'
                               WHEN 'waiting' THEN armed_at + $2 * INTERVAL '1 millisecond'
                           END
                       ),
                       session_id
              LIMIT 1",
        )
        .bind(admission_millis)
        .bind(waiting_millis)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_failure("select_deadline_candidate"))?;
        let Some(candidate) = candidate else {
            return Ok(SessionDeadlinePassOutcome::Idle);
        };
        let mut transaction = self.pool.begin().await.map_err(database_failure("begin"))?;
        let session = session_id_from_uuid(candidate);
        let held = session_lifecycle::load_locked(&mut transaction, session)
            .await
            .map_err(lifecycle_failure("load_locked_lifecycle"))?;
        let deadline = sqlx::query(
            "UPDATE session_deadline
                SET expires_at = CASE deadline_kind
                    WHEN 'admission' THEN armed_at + $2 * INTERVAL '1 millisecond'
                    WHEN 'waiting' THEN armed_at + $3 * INTERVAL '1 millisecond'
                END
             WHERE session_id = $1 AND NOT settled
         RETURNING session_deadline.deadline_kind,
                   session_deadline.expires_at <= clock_timestamp() AS due",
        )
        .bind(candidate)
        .bind(admission_millis)
        .bind(waiting_millis)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_failure("materialize_deadline_expiry"))?;
        let Some(deadline) = deadline else {
            transaction
                .rollback()
                .await
                .map_err(database_failure("rollback_missing_deadline"))?;
            return Ok(SessionDeadlinePassOutcome::Idle);
        };
        let kind: String = deadline
            .try_get("deadline_kind")
            .map_err(database_failure("decode_deadline_kind"))?;
        let due: Option<bool> = deadline
            .try_get("due")
            .map_err(database_failure("decode_deadline_due"))?;
        if due != Some(true) {
            transaction
                .commit()
                .await
                .map_err(database_failure("commit_armed_deadline"))?;
            return Ok(SessionDeadlinePassOutcome::Armed { session });
        }
        let outcome = match (kind.as_str(), held.state()) {
            ("admission", SessionLifecycleState::Created | SessionLifecycleState::Dispatched) => {
                sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SCHEDULER)
                    .bind(candidate)
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(database_failure("lock_admission_scheduler"))?;
                // Activation records lineage; retiring queued work does not.
                let has_activity: bool = sqlx::query_scalar(
                    "SELECT EXISTS (
                        SELECT 1 FROM turn_lifecycle
                         WHERE session_id = $1 AND start_lineage_kind IS NOT NULL
                    )",
                )
                .bind(candidate)
                .fetch_one(&mut *transaction)
                .await
                .map_err(database_failure("read_admission_activity"))?;
                if has_activity {
                    sqlx::query(
                        "UPDATE session_deadline
                            SET settled = true, expires_at = NULL
                          WHERE session_id = $1 AND deadline_kind = 'admission'",
                    )
                    .bind(candidate)
                    .execute(&mut *transaction)
                    .await
                    .map_err(database_failure("settle_superseded_admission_deadline"))?;
                    SessionDeadlinePassOutcome::Superseded { session }
                } else {
                    retire_queued_turns(&mut transaction, session)
                        .await
                        .map_err(database_failure("retire_queued_turns"))?;
                    session_lifecycle::close_in_transaction(
                        &mut transaction,
                        session,
                        SessionTerminalOutcome::Retired {
                            cause: SessionRetirementCause::AdmissionDeadlineExpired,
                        },
                        LifecycleActor::Watchdog,
                    )
                    .await
                    .map_err(lifecycle_failure("close_expired_admission"))?;
                    SessionDeadlinePassOutcome::Retired { session }
                }
            }
            ("waiting", SessionLifecycleState::Waiting { .. }) => {
                session_lifecycle::park_in_transaction(
                    &mut transaction,
                    session,
                    SessionParkCause::WaitingDeadlineExpired,
                    SessionParkResponder::Operator,
                    None,
                    LifecycleActor::Watchdog,
                )
                .await
                .map_err(lifecycle_failure("park_expired_waiting"))?;
                SessionDeadlinePassOutcome::Parked { session }
            }
            _ => {
                transaction
                    .rollback()
                    .await
                    .map_err(database_failure("rollback_inapplicable_deadline"))?;
                return Ok(SessionDeadlinePassOutcome::Idle);
            }
        };
        transaction.commit().await.map_err(|error| {
            if crate::commit_failure_is_ambiguous(&error) {
                lifecycle_failure("commit_expired_deadline")(
                    SessionLifecycleRepositoryError::CommitAmbiguous(error),
                )
            } else {
                database_failure("commit_expired_deadline")(error)
            }
        })?;
        Ok(outcome)
    }
}

fn database_failure(
    query: &'static str,
) -> impl FnOnce(sqlx::Error) -> SessionDeadlineRepositoryError {
    move |source| SessionDeadlineRepositoryError::Database { query, source }
}

fn lifecycle_failure(
    query: &'static str,
) -> impl FnOnce(SessionLifecycleRepositoryError) -> SessionDeadlineRepositoryError {
    move |source| SessionDeadlineRepositoryError::Lifecycle {
        query,
        source: Box::new(source),
    }
}

fn stored_millis(bound: Option<Duration>) -> Result<Option<i64>, SessionDeadlineRepositoryError> {
    bound
        .map(|bound| {
            i64::try_from(bound.as_millis())
                .map_err(|_| SessionDeadlineRepositoryError::BoundExceedsStorage)
        })
        .transpose()
}
