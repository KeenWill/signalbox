//! Dedicated PostgreSQL session guard for one hub per database.

use std::{error::Error, fmt, time::Duration};

use sqlx::{Connection, PgConnection, PgPool};
use tokio::time::{sleep, timeout};

const SIGNALBOX_GUARD_NAMESPACE: i32 = 1_396_856_881;
const HUB_GUARD_NAMESPACE: i32 = 1_213_547_057;
const GUARD_CHECK_TIMEOUT: Duration = Duration::from_secs(1);
/// Pause between dedicated-session guard pings, including timeout retries.
pub const GUARD_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const GUARD_CHECK_MISS_THRESHOLD: u8 = 3;

#[derive(Debug, Default)]
struct GuardCheckMisses {
    consecutive_timeouts: u8,
}

#[derive(Debug, Eq, PartialEq)]
enum GuardCheckStatus {
    Healthy,
    Retry,
}

impl GuardCheckMisses {
    fn record(
        &mut self,
        result: Result<(), SingleHubGuardError>,
    ) -> Result<GuardCheckStatus, SingleHubGuardError> {
        match result {
            Ok(()) => {
                self.consecutive_timeouts = 0;
                Ok(GuardCheckStatus::Healthy)
            }
            Err(SingleHubGuardError::GuardLost(None)) => {
                self.consecutive_timeouts = self.consecutive_timeouts.saturating_add(1);
                tracing::warn!(
                    miss_count = self.consecutive_timeouts,
                    timeout_seconds = GUARD_CHECK_TIMEOUT.as_secs(),
                    "database guard ping timed out"
                );
                if self.consecutive_timeouts >= GUARD_CHECK_MISS_THRESHOLD {
                    Err(SingleHubGuardError::GuardLost(None))
                } else {
                    Ok(GuardCheckStatus::Retry)
                }
            }
            Err(error) => Err(error),
        }
    }
}

/// One dedicated PostgreSQL session holding the database-scoped hub guard.
#[derive(Debug)]
pub struct SingleHubGuard {
    connection: PgConnection,
    misses: GuardCheckMisses,
}

impl SingleHubGuard {
    /// Attempts the fixed session-level advisory guard on a dedicated checkout.
    pub async fn acquire(pool: &PgPool) -> Result<Self, SingleHubGuardError> {
        let pooled = pool
            .acquire()
            .await
            .map_err(SingleHubGuardError::AcquireConnection)?;
        let mut connection = pooled.detach();
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, $2)")
            .bind(SIGNALBOX_GUARD_NAMESPACE)
            .bind(HUB_GUARD_NAMESPACE)
            .fetch_one(&mut connection)
            .await
            .map_err(SingleHubGuardError::AcquireLock)?;
        if !acquired {
            return Err(SingleHubGuardError::AlreadyRunning);
        }
        Ok(Self {
            connection,
            misses: GuardCheckMisses::default(),
        })
    }

    /// Proves that the exact guarded session remains usable.
    /// Retries short ping stalls, failing after three consecutive timeouts or
    /// immediately when a ping returns an error.
    pub async fn check(&mut self) -> Result<(), SingleHubGuardError> {
        loop {
            let result = match timeout(GUARD_CHECK_TIMEOUT, self.connection.ping()).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(SingleHubGuardError::GuardLost(Some(error))),
                Err(_) => Err(SingleHubGuardError::GuardLost(None)),
            };
            match self.misses.record(result)? {
                GuardCheckStatus::Healthy => return Ok(()),
                GuardCheckStatus::Retry => sleep(GUARD_CHECK_INTERVAL).await,
            }
        }
    }

    /// Closes the dedicated session, releasing the guard at graceful shutdown.
    pub async fn close(self) -> Result<(), SingleHubGuardError> {
        self.connection
            .close()
            .await
            .map_err(SingleHubGuardError::Close)
    }

    pub(crate) fn connection_mut(&mut self) -> &mut PgConnection {
        &mut self.connection
    }
}

/// Sanitized dedicated-guard acquisition, monitoring, or release failure.
#[derive(Debug)]
pub enum SingleHubGuardError {
    /// A dedicated connection could not be checked out.
    AcquireConnection(sqlx::Error),
    /// PostgreSQL could not evaluate the fixed guard attempt.
    AcquireLock(sqlx::Error),
    /// Another process already holds the guard for this database.
    AlreadyRunning,
    /// The exact session holding the guard failed or timed out.
    GuardLost(Option<sqlx::Error>),
    /// The dedicated session could not close gracefully.
    Close(sqlx::Error),
}

impl fmt::Display for SingleHubGuardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AcquireConnection(_) => "the single-hub connection could not be acquired",
            Self::AcquireLock(_) => "the single-hub guard could not be attempted",
            Self::AlreadyRunning => "another hub already holds the database guard",
            Self::GuardLost(_) => "the single-hub guard session was lost",
            Self::Close(_) => "the single-hub guard session could not close",
        })
    }
}

impl Error for SingleHubGuardError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AcquireConnection(error) | Self::AcquireLock(error) | Self::Close(error) => {
                Some(error)
            }
            Self::GuardLost(Some(error)) => Some(error),
            Self::AlreadyRunning | Self::GuardLost(None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GuardCheckMisses, GuardCheckStatus, SingleHubGuardError};

    #[test]
    fn guard_check_recovery_resets_consecutive_timeouts() {
        let mut misses = GuardCheckMisses::default();

        assert_eq!(
            misses
                .record(Err(SingleHubGuardError::GuardLost(None)))
                .unwrap(),
            GuardCheckStatus::Retry
        );
        assert_eq!(
            misses
                .record(Err(SingleHubGuardError::GuardLost(None)))
                .unwrap(),
            GuardCheckStatus::Retry
        );
        assert_eq!(misses.record(Ok(())).unwrap(), GuardCheckStatus::Healthy);
        assert_eq!(
            misses
                .record(Err(SingleHubGuardError::GuardLost(None)))
                .unwrap(),
            GuardCheckStatus::Retry
        );
        assert_eq!(
            misses
                .record(Err(SingleHubGuardError::GuardLost(None)))
                .unwrap(),
            GuardCheckStatus::Retry
        );
    }

    #[test]
    fn guard_check_three_consecutive_timeouts_lose_guard() {
        let mut misses = GuardCheckMisses::default();

        assert_eq!(
            misses
                .record(Err(SingleHubGuardError::GuardLost(None)))
                .unwrap(),
            GuardCheckStatus::Retry
        );
        assert_eq!(
            misses
                .record(Err(SingleHubGuardError::GuardLost(None)))
                .unwrap(),
            GuardCheckStatus::Retry
        );
        assert!(matches!(
            misses.record(Err(SingleHubGuardError::GuardLost(None))),
            Err(SingleHubGuardError::GuardLost(None))
        ));
    }

    #[test]
    fn guard_check_ping_error_loses_guard_immediately() {
        let mut misses = GuardCheckMisses::default();

        assert!(matches!(
            misses.record(Err(SingleHubGuardError::GuardLost(Some(sqlx::Error::Io(
                std::io::ErrorKind::ConnectionReset.into()
            ))))),
            Err(SingleHubGuardError::GuardLost(Some(sqlx::Error::Io(error))))
                if error.kind() == std::io::ErrorKind::ConnectionReset
        ));
    }
}
