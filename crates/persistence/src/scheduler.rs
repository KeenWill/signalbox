//! PostgreSQL reconciliation sweep for the application scheduler.

use std::{error::Error, fmt};

use signalbox_application::{ClassifyOperatorFailure, EligibilitySweep, OperatorFailureClass};
use signalbox_domain::SessionId;
use sqlx::{PgPool, types::Uuid};

use crate::mapping::session_id_from_uuid;

/// Infrastructure failure while reading reconciliation hints.
#[derive(Debug)]
pub struct PostgresEligibilitySweepError(sqlx::Error);

impl fmt::Display for PostgresEligibilitySweepError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "eligibility reconciliation query failed: {}",
            self.0
        )
    }
}

impl Error for PostgresEligibilitySweepError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

impl From<sqlx::Error> for PostgresEligibilitySweepError {
    fn from(error: sqlx::Error) -> Self {
        Self(error)
    }
}

impl ClassifyOperatorFailure for PostgresEligibilitySweepError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        }
    }
}

/// Finds durable sessions that may contain eligible queued work.
#[derive(Clone, Debug)]
pub struct PostgresEligibilitySweep {
    pool: PgPool,
}

impl PostgresEligibilitySweep {
    /// Uses the supplied pool for reconciliation queries.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Finds each session with queued work and no active slot owner.
    ///
    /// The result is only a set of hints. The authoritative per-session pass
    /// reconstitutes complete queue and lifecycle state under its scheduler-row
    /// lock before applying any transition.
    pub async fn find_sessions(&self) -> Result<Vec<SessionId>, PostgresEligibilitySweepError> {
        let sessions = sqlx::query_scalar::<_, Uuid>(
            "SELECT queued.session_id
               FROM turn_lifecycle AS queued
              WHERE queued.state_kind = 'queued'
                AND NOT EXISTS (
                    SELECT 1
                      FROM turn_lifecycle AS active
                     WHERE active.session_id = queued.session_id
                       AND active.state_kind = 'active'
                )
              GROUP BY queued.session_id
              ORDER BY queued.session_id",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(sessions.into_iter().map(session_id_from_uuid).collect())
    }
}

impl EligibilitySweep for PostgresEligibilitySweep {
    type Error = PostgresEligibilitySweepError;

    async fn find_sessions(&mut self) -> Result<Vec<SessionId>, Self::Error> {
        PostgresEligibilitySweep::find_sessions(self).await
    }
}
