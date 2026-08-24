//! PostgreSQL reconciliation sweep for the application scheduler.

use std::{collections::HashSet, error::Error, fmt};

use signalbox_application::{
    ClassifyOperatorFailure, EligibilitySweep, EligibilitySweepBatch, OperatorFailureClass,
};
use signalbox_domain::SessionId;
use sqlx::{PgPool, types::Uuid};

use crate::mapping::{session_id_from_uuid, session_id_to_uuid};

const RECONCILIATION_PAGE_SIZE: i64 = 16;

fn next_page_state(
    rows: &[(SessionId, SessionId, bool)],
) -> (Option<SessionId>, Option<SessionId>) {
    let Some((last_session, scan_through, _)) = rows.last().copied() else {
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

/// Finds durable sessions that may require an authoritative scheduling pass.
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

    /// Finds the next bounded page of sessions with queued work, a durable
    /// active tool batch, or a terminal goal turn owed disposition.
    ///
    /// The result is only a set of hints. The authoritative per-session pass
    /// reconstitutes complete queue and lifecycle state under its scheduler-row
    /// lock before applying any transition.
    pub async fn find_sessions(
        &mut self,
    ) -> Result<EligibilitySweepBatch, PostgresEligibilitySweepError> {
        let after = self.after.map(session_id_to_uuid);
        let scan_through = self.scan_through.map(session_id_to_uuid);
        let rows = sqlx::query_as::<_, (Uuid, Uuid, bool)>(
            "WITH candidates AS (
                SELECT queued.session_id
                  FROM turn_lifecycle AS queued
                 WHERE queued.state_kind = 'queued'
                   AND goal_turn_is_runtime_relevant(
                       queued.session_id,
                       queued.turn_id
                   )
                   AND NOT EXISTS (
                       SELECT 1
                         FROM turn_lifecycle AS active
                        WHERE active.session_id = queued.session_id
                          AND active.state_kind = 'active'
                          AND NOT active.delegation_runtime_terminal
                 )
                 GROUP BY queued.session_id
                UNION
                SELECT lease.session_id
                  FROM repo_watch_dispatch_start_lease AS lease
                 WHERE lease.expires_at > clock_timestamp()
                   AND NOT EXISTS (
                       SELECT 1 FROM model_call AS call
                        WHERE call.session_id = lease.session_id
                   )
                   AND NOT EXISTS (
                       SELECT 1
                         FROM repo_watch_dispatch_start_lease_expiration AS expired
                        WHERE expired.dispatch_id = lease.dispatch_id
                          AND expired.action_ordinal = lease.action_ordinal
                   )
                   AND NOT EXISTS (
                       SELECT 1
                         FROM repo_watch_dispatch_start_lease_quarantine AS quarantined
                        WHERE quarantined.dispatch_id = lease.dispatch_id
                          AND quarantined.action_ordinal = lease.action_ordinal
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM repo_watch_dispatch_release AS released
                        WHERE released.dispatch_id = lease.dispatch_id
                   )
                UNION
                SELECT terminal.session_id
                  FROM turn_lifecycle AS terminal
                  JOIN goal_turn AS terminal_goal
                    ON terminal_goal.session_id = terminal.session_id
                   AND terminal_goal.turn_id = terminal.turn_id
                  JOIN goal_event AS current_event
                    ON current_event.session_id = terminal.session_id
                 WHERE terminal.state_kind = 'terminal'
                   AND current_event.event_kind IN (
                       'commissioned', 'resumed', 'superseded'
                   )
                   AND NOT EXISTS (
                       SELECT 1
                         FROM goal_event AS later_event
                        WHERE later_event.session_id = current_event.session_id
                          AND later_event.event_ordinal > current_event.event_ordinal
                   )
                   AND NOT EXISTS (
                       SELECT 1
                         FROM goal_turn AS later_goal
                         JOIN turn_lifecycle AS later_lifecycle
                           ON later_lifecycle.session_id = later_goal.session_id
                          AND later_lifecycle.turn_id = later_goal.turn_id
                        WHERE later_goal.session_id = terminal_goal.session_id
                          AND later_lifecycle.acceptance_position
                              > terminal.acceptance_position
                   )
                 GROUP BY terminal.session_id
                UNION
                SELECT active.session_id
                  FROM turn_lifecycle AS active
                 WHERE active.state_kind = 'active'
                   AND NOT active.delegation_runtime_terminal
                   AND EXISTS (
                        SELECT 1
                          FROM model_call AS call
                         WHERE call.session_id = active.session_id
                           AND call.turn_id = active.turn_id
                           AND call.state_kind = 'prepared'
                   )
                UNION
                SELECT active.session_id
                  FROM turn_lifecycle AS active
                 WHERE active.state_kind = 'active'
                   AND NOT active.delegation_runtime_terminal
                   AND active.active_tool_round_call_id IS NOT NULL
                   AND (
                        active.active_phase_kind = 'running'
                        OR (
                            active.active_phase_kind = 'awaiting_tool_approval'
                            AND EXISTS (
                                SELECT 1
                                  FROM tool_request AS request
                                 WHERE request.request_id =
                                       active.approval_tool_request_id
                                   AND request.session_id = active.session_id
                                   AND request.turn_id = active.turn_id
                                   AND request.approval_posture = 'delegated'
                                   AND NOT EXISTS (
                                        SELECT 1
                                          FROM tool_approval_judge_model_call AS judge
                                         WHERE judge.request_id = request.request_id
                                           AND judge.state_kind = 'terminal'
                                   )
                            )
                        )
                        OR (
                            active.active_phase_kind = 'awaiting_child'
                            AND EXISTS (
                                SELECT 1
                                  FROM session_delegation_wait AS waiting
                                  JOIN session_child_result_delivery AS delivery
                                    ON delivery.awaiting_tool_request_id =
                                       waiting.awaiting_tool_request_id
                                   AND delivery.spawning_tool_request_id =
                                       waiting.spawning_tool_request_id
                                   AND delivery.parent_session_id =
                                       waiting.parent_session_id
                                   AND delivery.delivery_sequence IS NULL
                                 WHERE waiting.awaiting_tool_request_id =
                                       active.child_wait_request_id
                                   AND waiting.parent_session_id = active.session_id
                                   AND waiting.parent_turn_id = active.turn_id
                                   AND waiting.wait_mode = 'foreground'
                            )
                        )
                   )
             ), bounded AS (
                SELECT COALESCE(
                    $2::uuid,
                    (SELECT session_id
                       FROM candidates
                      ORDER BY session_id DESC
                      LIMIT 1)
                ) AS scan_through
             )
             SELECT candidates.session_id, bounded.scan_through,
                    EXISTS (
                        SELECT 1
                          FROM repo_watch_dispatch_start_lease AS lease
                         WHERE lease.session_id = candidates.session_id
                           AND lease.expires_at > clock_timestamp()
                           AND NOT EXISTS (SELECT 1 FROM model_call AS call WHERE call.session_id = lease.session_id)
                           AND NOT EXISTS (SELECT 1 FROM repo_watch_dispatch_start_lease_expiration AS expired WHERE expired.dispatch_id = lease.dispatch_id AND expired.action_ordinal = lease.action_ordinal)
                           AND NOT EXISTS (SELECT 1 FROM repo_watch_dispatch_start_lease_quarantine AS quarantined WHERE quarantined.dispatch_id = lease.dispatch_id AND quarantined.action_ordinal = lease.action_ordinal)
                           AND NOT EXISTS (SELECT 1 FROM repo_watch_dispatch_release AS released WHERE released.dispatch_id = lease.dispatch_id)
                    ) AS dispatch_start
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
            .map(|(session, scan_through, dispatch_start)| {
                (
                    session_id_from_uuid(session),
                    session_id_from_uuid(scan_through),
                    dispatch_start,
                )
            })
            .collect::<Vec<_>>();
        let next_state = next_page_state(&rows);
        let continuation = next_state.0.is_some();
        (self.after, self.scan_through) = next_state;
        let dispatch_starts = rows
            .iter()
            .filter_map(|(session, _, priority)| (*priority).then_some(*session))
            .collect::<HashSet<_>>();
        Ok(EligibilitySweepBatch::with_dispatch_starts(
            rows.into_iter().map(|(session, _, _)| session).collect(),
            dispatch_starts,
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
            .map(|session| (session, beyond_page, false))
            .collect::<Vec<_>>();
        let cycle_end = sessions
            .iter()
            .copied()
            .map(|session| (session, *sessions.last().expect("page is nonempty"), false))
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
