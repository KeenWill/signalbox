//! Periodic application of durable session admission and waiting deadlines.

use signalbox_application::TurnLivenessScanInterval;
use signalbox_persistence::session_deadline::{
    PostgresSessionDeadlineRepository, SessionDeadlineBounds, SessionDeadlinePassOutcome,
    SessionDeadlineRepositoryError,
};
use sqlx::PgPool;
use tokio::{
    select,
    sync::watch,
    time::{MissedTickBehavior, interval},
};

/// Core deadline expiry on the existing liveness cadence.
#[derive(Clone, Debug)]
pub struct LifecycleDeadlineRuntime {
    repository: PostgresSessionDeadlineRepository,
    scan_interval: Option<TurnLivenessScanInterval>,
}

impl LifecycleDeadlineRuntime {
    /// Uses the durable session deadline store and existing configured cadence.
    pub const fn new(
        pool: PgPool,
        scan_interval: Option<TurnLivenessScanInterval>,
        bounds: SessionDeadlineBounds,
    ) -> Self {
        Self {
            repository: PostgresSessionDeadlineRepository::new(pool, bounds),
            scan_interval,
        }
    }

    /// Applies due transitions until shutdown.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let Some(scan_interval) = self.scan_interval else {
            while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
            return;
        };
        let mut ticker = interval(scan_interval.get());
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow_and_update() {
                        return;
                    }
                }
                _ = ticker.tick() => {
                    loop {
                        match self.repository.expire_next().await {
                            Ok(SessionDeadlinePassOutcome::Idle) => break,
                            Ok(SessionDeadlinePassOutcome::Armed { .. }) => {}
                            Ok(SessionDeadlinePassOutcome::Superseded { session }) => {
                                tracing::info!(session_id = %session.into_uuid(),
                                    "session activity superseded the admission deadline");
                            }
                            Ok(SessionDeadlinePassOutcome::Retired { session }) => {
                                tracing::info!(session_id = %session.into_uuid(),
                                    "session admission deadline retired the session");
                            }
                            Ok(SessionDeadlinePassOutcome::Parked { session }) => {
                                tracing::info!(session_id = %session.into_uuid(),
                                    "session waiting deadline parked the session");
                            }
                            Err(error) => {
                                warn_deadline_failure(&error);
                                break;
                            }
                        }
                        if *shutdown.borrow() {
                            return;
                        }
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
    }
}

// Only operation labels and closed classes cross the diagnostic boundary in
// docs/spec/process-protocol.md; SQLx's Display and Debug retain source data.
fn warn_deadline_failure(error: &SessionDeadlineRepositoryError) {
    use signalbox_persistence::session_lifecycle::SessionLifecycleRepositoryError;

    let (operation, error_class, database) = match error {
        SessionDeadlineRepositoryError::BoundExceedsStorage => {
            ("decode_deadline_bound", "bound_exceeds_storage", None)
        }
        SessionDeadlineRepositoryError::Database { query, source } => {
            (*query, sqlx_error_class(source), Some(source))
        }
        SessionDeadlineRepositoryError::Lifecycle { query, source } => match source.as_ref() {
            SessionLifecycleRepositoryError::Database(error) => {
                (*query, sqlx_error_class(error), Some(error))
            }
            SessionLifecycleRepositoryError::CommitAmbiguous(error) => {
                (*query, "commit_ambiguous", Some(error))
            }
            SessionLifecycleRepositoryError::UnknownSession(_) => (*query, "unknown_session", None),
            SessionLifecycleRepositoryError::Rejected(_) => (*query, "lifecycle_rejected", None),
            SessionLifecycleRepositoryError::Goal(_) => (*query, "goal_failure", None),
            SessionLifecycleRepositoryError::Corruption(_) => {
                (*query, "lifecycle_corruption", None)
            }
        },
    };
    let sqlstate = database
        .and_then(sqlx::Error::as_database_error)
        .and_then(|error| error.try_downcast_ref::<sqlx::postgres::PgDatabaseError>())
        .map(sqlx::postgres::PgDatabaseError::code)
        // PostgreSQL SQLSTATE is exactly five uppercase ASCII letters/digits.
        .filter(|code| {
            code.len() == 5
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        });
    tracing::warn!(
        operation,
        error_class,
        sqlstate,
        "session deadline pass produced no decision"
    );
}

fn sqlx_error_class(error: &sqlx::Error) -> &'static str {
    match error {
        sqlx::Error::Database(_) => "database",
        sqlx::Error::PoolTimedOut => "pool_timed_out",
        sqlx::Error::PoolClosed => "pool_closed",
        sqlx::Error::BeginFailed => "begin_failed",
        sqlx::Error::Io(_) => "io",
        sqlx::Error::Tls(_) => "tls",
        sqlx::Error::Protocol(_) => "protocol",
        sqlx::Error::RowNotFound => "row_not_found",
        sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_) => "decode",
        sqlx::Error::Encode(_) => "encode",
        sqlx::Error::ColumnNotFound(_)
        | sqlx::Error::ColumnIndexOutOfBounds { .. }
        | sqlx::Error::TypeNotFound { .. } => "result_shape",
        sqlx::Error::Configuration(_)
        | sqlx::Error::InvalidArgument(_)
        | sqlx::Error::InvalidSavePointStatement => "invalid_request",
        sqlx::Error::WorkerCrashed => "worker_crashed",
        _ => "sqlx_other",
    }
}
