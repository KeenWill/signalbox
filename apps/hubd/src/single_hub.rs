//! Dedicated PostgreSQL session guard for one hub per database.

use std::{error::Error, fmt};

use sqlx::{Connection, PgConnection, PgPool};

const SIGNALBOX_GUARD_NAMESPACE: i32 = 1_396_856_881;
const HUB_GUARD_NAMESPACE: i32 = 1_213_547_057;

/// One dedicated PostgreSQL session holding the database-scoped hub guard.
#[derive(Debug)]
pub struct SingleHubGuard {
    connection: PgConnection,
}

impl SingleHubGuard {
    /// Attempts the fixed session-level advisory guard on a dedicated checkout.
    pub async fn acquire(pool: &PgPool) -> Result<Self, SingleHubGuardError> {
        let mut pooled = pool
            .acquire()
            .await
            .map_err(SingleHubGuardError::AcquireConnection)?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, $2)")
            .bind(SIGNALBOX_GUARD_NAMESPACE)
            .bind(HUB_GUARD_NAMESPACE)
            .fetch_one(&mut *pooled)
            .await
            .map_err(SingleHubGuardError::AcquireLock)?;
        if !acquired {
            return Err(SingleHubGuardError::AlreadyRunning);
        }
        Ok(Self {
            connection: pooled.detach(),
        })
    }

    /// Proves that the exact guarded session remains usable.
    pub async fn check(&mut self) -> Result<(), SingleHubGuardError> {
        self.connection
            .ping()
            .await
            .map_err(SingleHubGuardError::GuardLost)
    }

    /// Closes the dedicated session, releasing the guard at graceful shutdown.
    pub async fn close(self) -> Result<(), SingleHubGuardError> {
        self.connection
            .close()
            .await
            .map_err(SingleHubGuardError::Close)
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
    /// The exact session holding the guard was lost.
    GuardLost(sqlx::Error),
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
            Self::AcquireConnection(error)
            | Self::AcquireLock(error)
            | Self::GuardLost(error)
            | Self::Close(error) => Some(error),
            Self::AlreadyRunning => None,
        }
    }
}
