//! Fail-closed bridge to the commissioned startup-recovery implementation.
//!
//! The hub composition-root slice must not schedule around prior-process work.
//! Until the immediately stacked INV-034 slice supplies the terminalizing scan,
//! this adapter proves the durable inventory is clean or blocks startup.

use std::{error::Error, fmt};

use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use sqlx::PgPool;

/// Result of checking whether the terminalizing startup scan has work to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupRecoveryBarrierOutcome {
    /// No active turn remains from an earlier process.
    Clean,
    /// Scheduling must wait for the terminalizing startup-scan slice.
    RecoveryRequired {
        /// Number of active turns requiring recovery.
        active_turn_count: u64,
    },
}

/// Failure while reading the startup-recovery inventory.
#[derive(Debug)]
pub enum StartupRecoveryBarrierError {
    /// PostgreSQL could not read the inventory.
    Database(sqlx::Error),
    /// PostgreSQL returned a count outside the application representation.
    Corruption(&'static str),
}

impl fmt::Display for StartupRecoveryBarrierError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "startup inventory query failed: {error}"),
            Self::Corruption(relationship) => {
                write!(
                    formatter,
                    "startup inventory is inconsistent: {relationship}"
                )
            }
        }
    }
}

impl Error for StartupRecoveryBarrierError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for StartupRecoveryBarrierError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl ClassifyOperatorFailure for StartupRecoveryBarrierError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
        }
    }
}

/// Checks whether scheduling may start before recovery is implemented.
#[derive(Clone, Debug)]
pub struct PostgresStartupRecoveryBarrier {
    pool: PgPool,
}

impl PostgresStartupRecoveryBarrier {
    /// Uses the supplied shared pool for the startup inventory read.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Returns `Clean` only when no active turn requires INV-034 recovery.
    pub async fn inspect(
        &self,
    ) -> Result<StartupRecoveryBarrierOutcome, StartupRecoveryBarrierError> {
        let active_turn_count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*)
               FROM turn_lifecycle
              WHERE state_kind = 'active'",
        )
        .fetch_one(&self.pool)
        .await?;
        let active_turn_count = u64::try_from(active_turn_count)
            .map_err(|_| StartupRecoveryBarrierError::Corruption("negative active-turn count"))?;

        Ok(if active_turn_count == 0 {
            StartupRecoveryBarrierOutcome::Clean
        } else {
            StartupRecoveryBarrierOutcome::RecoveryRequired { active_turn_count }
        })
    }
}
