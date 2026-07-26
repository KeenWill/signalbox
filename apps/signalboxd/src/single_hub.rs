//! Dedicated PostgreSQL session guard for one hub per database.

use std::{error::Error, fmt, time::Duration};

use sqlx::{Connection, PgConnection, PgPool};
use tokio::time::timeout;

const SIGNALBOX_GUARD_NAMESPACE: i32 = 1_396_856_881;
const HUB_GUARD_NAMESPACE: i32 = 1_213_547_057;
const GUARD_CHECK_TIMEOUT: Duration = Duration::from_secs(1);

/// One dedicated PostgreSQL session holding the database-scoped hub guard.
#[derive(Debug)]
pub struct SingleHubGuard {
    connection: PgConnection,
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
        Ok(Self { connection })
    }

    /// Proves that the exact guarded session remains usable.
    pub async fn check(&mut self) -> Result<(), SingleHubGuardError> {
        match timeout(GUARD_CHECK_TIMEOUT, self.connection.ping()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(SingleHubGuardError::GuardLost(Some(error))),
            Err(_) => Err(SingleHubGuardError::GuardLost(None)),
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
