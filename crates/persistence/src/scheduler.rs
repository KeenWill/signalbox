//! PostgreSQL reconciliation sweep for the application scheduler.

use std::{error::Error, fmt};

use signalbox_application::{
    ClassifyOperatorFailure, EligibilitySweep, EligibilitySweepBatch, OperatorFailureClass,
};
use signalbox_domain::SessionId;
use sqlx::{PgPool, types::Uuid};

use crate::mapping::{session_id_from_uuid, session_id_to_uuid};

const RECONCILIATION_PAGE_SIZE: i64 = 16;

fn next_page_state(rows: &[(SessionId, SessionId)]) -> (Option<SessionId>, Option<SessionId>) {
    let Some((last_session, scan_through)) = rows.last().copied() else {
        return (None, None);
    };
    if rows.len() == RECONCILIATION_PAGE_SIZE as usize && last_session != scan_through {
        (Some(last_session), Some(scan_through))
    } else {
        (None, None)
    }
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
    scan_through: Option<SessionId>,
}

impl PostgresEligibilitySweep {
    /// Uses the supplied pool for reconciliation queries.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            after: None,
            scan_through: None,
        }
    }

    /// Finds the next bounded page of sessions with queued work and no active
    /// slot owner.
    ///
    /// The result is only a set of hints. The authoritative per-session pass
    /// reconstitutes complete queue and lifecycle state under its scheduler-row
    /// lock before applying any transition.
    pub async fn find_sessions(
        &mut self,
    ) -> Result<EligibilitySweepBatch, PostgresEligibilitySweepError> {
        let after = self.after.map(session_id_to_uuid);
        let scan_through = self.scan_through.map(session_id_to_uuid);
        let rows = sqlx::query_as::<_, (Uuid, Uuid)>(
            "WITH candidates AS (
                SELECT queued.session_id
                  FROM turn_lifecycle AS queued
                 WHERE queued.state_kind = 'queued'
                   AND NOT EXISTS (
                       SELECT 1
                         FROM turn_lifecycle AS active
                        WHERE active.session_id = queued.session_id
                          AND active.state_kind = 'active'
                   )
                 GROUP BY queued.session_id
             ), bounded AS (
                SELECT COALESCE(
                    $2::uuid,
                    (SELECT session_id
                       FROM candidates
                      ORDER BY session_id DESC
                      LIMIT 1)
                ) AS scan_through
             )
             SELECT candidates.session_id, bounded.scan_through
               FROM candidates
               CROSS JOIN bounded
              WHERE bounded.scan_through IS NOT NULL
                AND ($1::uuid IS NULL OR candidates.session_id > $1)
                AND candidates.session_id <= bounded.scan_through
              ORDER BY candidates.session_id
              LIMIT $3",
        )
        .bind(after)
        .bind(scan_through)
        .bind(RECONCILIATION_PAGE_SIZE)
        .fetch_all(&self.pool)
        .await?;

        let rows = rows
            .into_iter()
            .map(|(session, scan_through)| {
                (
                    session_id_from_uuid(session),
                    session_id_from_uuid(scan_through),
                )
            })
            .collect::<Vec<_>>();
        let next_state = next_page_state(&rows);
        let continuation = next_state.0.is_some();
        (self.after, self.scan_through) = next_state;
        Ok(EligibilitySweepBatch::new(
            rows.into_iter().map(|(session, _)| session).collect(),
            continuation,
        ))
    }
}

impl EligibilitySweep for PostgresEligibilitySweep {
    type Error = PostgresEligibilitySweepError;

    async fn find_sessions(&mut self) -> Result<EligibilitySweepBatch, Self::Error> {
        self.find_sessions().await
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::SessionId;
    use sqlx::types::Uuid;

    use super::{RECONCILIATION_PAGE_SIZE, next_page_state};

    #[test]
    fn reconciliation_page_size_matches_scheduler_pass_bound() {
        assert_eq!(RECONCILIATION_PAGE_SIZE, 16);
    }

    #[test]
    fn pages_advance_only_until_the_fixed_cycle_bound() {
        let sessions = (1..=RECONCILIATION_PAGE_SIZE as u128)
            .map(|value| SessionId::from_uuid(Uuid::from_u128(value)))
            .collect::<Vec<_>>();
        let beyond_page = SessionId::from_uuid(Uuid::from_u128(17));
        let continuing = sessions
            .iter()
            .copied()
            .map(|session| (session, beyond_page))
            .collect::<Vec<_>>();
        let cycle_end = sessions
            .iter()
            .copied()
            .map(|session| (session, *sessions.last().expect("page is nonempty")))
            .collect::<Vec<_>>();

        assert_eq!(
            next_page_state(&continuing),
            (sessions.last().copied(), Some(beyond_page))
        );
        assert_eq!(next_page_state(&cycle_end), (None, None));
        assert_eq!(
            next_page_state(&continuing[..continuing.len() - 1]),
            (None, None)
        );
    }
}
