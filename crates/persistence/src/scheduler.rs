//! PostgreSQL reconciliation sweep for the application scheduler.

use std::{error::Error, fmt};

use signalbox_application::{ClassifyOperatorFailure, EligibilitySweep, OperatorFailureClass};
use signalbox_domain::SessionId;
use sqlx::{PgPool, types::Uuid};

use crate::mapping::{session_id_from_uuid, session_id_to_uuid};

const RECONCILIATION_PAGE_SIZE: i64 = 16;

fn next_page_cursor(sessions: &[SessionId]) -> Option<SessionId> {
    (sessions.len() == RECONCILIATION_PAGE_SIZE as usize)
        .then(|| *sessions.last().expect("a full page is nonempty"))
}

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
    after: Option<SessionId>,
}

impl PostgresEligibilitySweep {
    /// Uses the supplied pool for reconciliation queries.
    pub fn new(pool: PgPool) -> Self {
        Self { pool, after: None }
    }

    /// Finds the next bounded page of sessions with queued work and no active
    /// slot owner.
    ///
    /// The result is only a set of hints. The authoritative per-session pass
    /// reconstitutes complete queue and lifecycle state under its scheduler-row
    /// lock before applying any transition.
    pub async fn find_sessions(&mut self) -> Result<Vec<SessionId>, PostgresEligibilitySweepError> {
        let after = self.after.map(session_id_to_uuid);
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
                AND ($1::uuid IS NULL OR queued.session_id > $1)
              GROUP BY queued.session_id
              ORDER BY queued.session_id
              LIMIT $2",
        )
        .bind(after)
        .bind(RECONCILIATION_PAGE_SIZE)
        .fetch_all(&self.pool)
        .await?;

        let sessions = sessions
            .into_iter()
            .map(session_id_from_uuid)
            .collect::<Vec<_>>();
        self.after = next_page_cursor(&sessions);
        Ok(sessions)
    }
}

impl EligibilitySweep for PostgresEligibilitySweep {
    type Error = PostgresEligibilitySweepError;

    async fn find_sessions(&mut self) -> Result<Vec<SessionId>, Self::Error> {
        self.find_sessions().await
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::SessionId;
    use sqlx::types::Uuid;

    use super::{RECONCILIATION_PAGE_SIZE, next_page_cursor};

    #[test]
    fn reconciliation_page_size_matches_scheduler_pass_bound() {
        assert_eq!(RECONCILIATION_PAGE_SIZE, 16);
    }

    #[test]
    fn full_pages_advance_and_short_pages_restart_the_scan() {
        let sessions = (1..=RECONCILIATION_PAGE_SIZE as u128)
            .map(|value| SessionId::from_uuid(Uuid::from_u128(value)))
            .collect::<Vec<_>>();

        assert_eq!(next_page_cursor(&sessions), sessions.last().copied());
        assert_eq!(next_page_cursor(&sessions[..sessions.len() - 1]), None);
    }
}
