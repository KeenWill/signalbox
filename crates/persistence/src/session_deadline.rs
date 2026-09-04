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
    Database(sqlx::Error),
    /// The lifecycle transition failed beneath the pass.
    Lifecycle(Box<SessionLifecycleRepositoryError>),
}

impl fmt::Display for SessionDeadlineRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BoundExceedsStorage => {
                formatter.write_str("session deadline bound exceeds storage")
            }
            Self::Database(_) => formatter.write_str("session deadline database failure"),
            Self::Lifecycle(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for SessionDeadlineRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Lifecycle(error) => Some(error.as_ref()),
            Self::BoundExceedsStorage => None,
        }
    }
}

impl From<sqlx::Error> for SessionDeadlineRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SessionLifecycleRepositoryError> for SessionDeadlineRepositoryError {
    fn from(error: SessionLifecycleRepositoryError) -> Self {
        Self::Lifecycle(Box::new(error))
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
        let mut transaction = self.pool.begin().await?;
        let candidate: Option<Uuid> = sqlx::query_scalar(
            "SELECT session_id
               FROM session_deadline
              WHERE (deadline_kind = 'admission'
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
                          OR expires_at <= clock_timestamp()))
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
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(candidate) = candidate else {
            transaction.rollback().await?;
            return Ok(SessionDeadlinePassOutcome::Idle);
        };
        let session = session_id_from_uuid(candidate);
        let held = session_lifecycle::load_locked(&mut transaction, session).await?;
        let deadline = sqlx::query(
            "UPDATE session_deadline
                SET expires_at = CASE deadline_kind
                    WHEN 'admission' THEN armed_at + $2 * INTERVAL '1 millisecond'
                    WHEN 'waiting' THEN armed_at + $3 * INTERVAL '1 millisecond'
                END
             WHERE session_id = $1
         RETURNING session_deadline.deadline_kind,
                   session_deadline.expires_at <= clock_timestamp() AS due",
        )
        .bind(candidate)
        .bind(admission_millis)
        .bind(waiting_millis)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(deadline) = deadline else {
            transaction.rollback().await?;
            return Ok(SessionDeadlinePassOutcome::Idle);
        };
        let kind: String = deadline.try_get("deadline_kind")?;
        let due: Option<bool> = deadline.try_get("due")?;
        if due != Some(true) {
            transaction.commit().await?;
            return Ok(SessionDeadlinePassOutcome::Armed { session });
        }
        let outcome = match (kind.as_str(), held.state()) {
            ("admission", SessionLifecycleState::Created | SessionLifecycleState::Dispatched) => {
                sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SCHEDULER)
                    .bind(candidate)
                    .fetch_one(&mut *transaction)
                    .await?;
                retire_queued_turns(&mut transaction, session).await?;
                session_lifecycle::close_in_transaction(
                    &mut transaction,
                    session,
                    SessionTerminalOutcome::Retired {
                        cause: SessionRetirementCause::AdmissionDeadlineExpired,
                    },
                    LifecycleActor::Watchdog,
                )
                .await?;
                SessionDeadlinePassOutcome::Retired { session }
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
                .await?;
                SessionDeadlinePassOutcome::Parked { session }
            }
            _ => {
                transaction.rollback().await?;
                return Ok(SessionDeadlinePassOutcome::Idle);
            }
        };
        transaction.commit().await.map_err(|error| {
            if crate::commit_failure_is_ambiguous(&error) {
                SessionDeadlineRepositoryError::Lifecycle(Box::new(
                    SessionLifecycleRepositoryError::CommitAmbiguous(error),
                ))
            } else {
                SessionDeadlineRepositoryError::Database(error)
            }
        })?;
        Ok(outcome)
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
