//! Durable observations for bounded repository-watch compaction successors.

use signalbox_domain::{DurableCommandId, SessionId, TurnId};
use sqlx::{FromRow, PgPool, types::Uuid};

/// Persistence boundary for terminal continuation and compaction receipts.
#[derive(Clone, Debug)]
pub struct CompactionContinuationRepository {
    pool: PgPool,
}

#[derive(FromRow)]
struct StoredSuccessor {
    accepting_command_id: Option<Uuid>,
    previous_turn: Uuid,
    goal_continuation: bool,
    compacted: bool,
}

impl CompactionContinuationRepository {
    /// Uses the daemon's session pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Finds a terminal headroom failure with no later accepted work.
    pub async fn terminal_candidate(
        &self,
        session: SessionId,
    ) -> Result<Option<TurnId>, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT failed.turn_id
               FROM turn_lifecycle AS failed
               JOIN tool_continuation_context_headroom AS headroom
                 ON headroom.terminal_attempt_id = failed.terminal_attempt_id
              WHERE failed.session_id = $1
                AND NOT EXISTS (
                    SELECT 1 FROM turn_lifecycle AS later
                     WHERE later.session_id = failed.session_id
                       AND later.acceptance_position > failed.acceptance_position)",
        )
        .bind(session.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map(|turn| turn.map(TurnId::from_uuid))
    }

    /// Whether the bounded ordinary-input command already has a durable result.
    pub async fn command_recorded(&self, command: DurableCommandId) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM durable_command WHERE command_id = $1)")
            .bind(command.into_uuid())
            .fetch_one(&self.pool)
            .await
    }

    /// Whether this successor still owes compaction before activation.
    pub async fn requires_compaction(
        &self,
        session: SessionId,
        turn: TurnId,
        ordinary_command: impl FnOnce(TurnId) -> DurableCommandId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, StoredSuccessor>(
            "SELECT origin.accepting_command_id, previous.turn_id AS previous_turn,
                    EXISTS (SELECT 1 FROM goal_turn AS goal
                             WHERE goal.session_id = queued.session_id
                               AND goal.turn_id = queued.turn_id
                               AND goal.predecessor_turn_id = previous.turn_id) AS goal_continuation,
                    EXISTS (SELECT 1 FROM compact_session_command AS compact
                             WHERE compact.session_id = queued.session_id
                               AND compact.automatic_for_turn_id = queued.turn_id
                               AND compact.result_kind = 'applied') AS compacted
               FROM turn_lifecycle AS queued
               JOIN session AS owner ON owner.session_id = queued.session_id
                AND owner.creation_cause = 'module_dispatched'
                AND owner.dispatching_module = 'repo_watch'
               JOIN accepted_input AS origin
                 ON origin.accepted_input_id = queued.origin_accepted_input_id
               JOIN LATERAL (
                   SELECT failed.turn_id
                     FROM turn_lifecycle AS failed
                     JOIN tool_continuation_context_headroom AS headroom
                       ON headroom.terminal_attempt_id = failed.terminal_attempt_id
                    WHERE failed.session_id = queued.session_id
                      AND failed.acceptance_position < queued.acceptance_position
                    ORDER BY failed.acceptance_position DESC LIMIT 1
               ) AS previous ON TRUE
              WHERE queued.session_id = $1 AND queued.turn_id = $2",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some_and(|row| {
            !row.compacted
                && (row.goal_continuation
                    || row.accepting_command_id
                        == Some(ordinary_command(TurnId::from_uuid(row.previous_turn)).into_uuid()))
        }))
    }
}
