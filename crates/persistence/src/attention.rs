//! PostgreSQL adapter for coherent bounded fleet attention reads.

use std::{collections::BTreeSet, error::Error, fmt, time::SystemTime};

use signalbox_application::{
    AttentionAction, AttentionActivity, AttentionActivityKind, AttentionBlockedReason,
    AttentionChanges, AttentionCursor, AttentionGoalBlock, AttentionJudgeFacts, AttentionReader,
    AttentionSnapshot, AttentionState, AttentionSummary, max_attention_change_items,
    max_attention_goal_summary_characters, max_attention_snapshot_items,
};
use signalbox_domain::{GoalBlockedReasonKind, SessionId, TurnId};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::mapping::goal_blocked_reason_from_str;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttentionCorruption {
    Missing(&'static str),
    Invalid(&'static str),
    Unsupported { field: &'static str, value: String },
}

impl fmt::Display for AttentionCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing operator attention {field}"),
            Self::Invalid(field) => write!(formatter, "invalid operator attention {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported operator attention {field}: {value}")
            }
        }
    }
}

impl Error for AttentionCorruption {}

#[derive(Debug)]
pub enum AttentionRepositoryError {
    Database(sqlx::Error),
    Corruption(AttentionCorruption),
}

impl fmt::Display for AttentionRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "attention database failure: {error}"),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for AttentionRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for AttentionRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<AttentionCorruption> for AttentionRepositoryError {
    fn from(error: AttentionCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// Read-only PostgreSQL implementation of the fleet projection port.
#[derive(Clone, Debug)]
pub struct AttentionRepository {
    pool: PgPool,
}

impl AttentionRepository {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn snapshot(
        &self,
        after: Option<SessionId>,
    ) -> Result<AttentionSnapshot, AttentionRepositoryError> {
        let mut transaction = self.read_transaction().await?;
        let cursor = current_cursor(&mut transaction).await?;
        let mut summaries = load_summaries(&mut transaction, None, after).await?;
        let has_more = summaries.len() > usize::from(max_attention_snapshot_items());
        summaries.truncate(usize::from(max_attention_snapshot_items()));
        let continuation_after = has_more
            .then(|| summaries.last().map(|row| row.session))
            .flatten();
        transaction.commit().await?;
        Ok(AttentionSnapshot {
            cursor,
            summaries,
            continuation_after,
        })
    }

    pub async fn changes_after(
        &self,
        cursor: AttentionCursor,
    ) -> Result<AttentionChanges, AttentionRepositoryError> {
        let mut transaction = self.read_transaction().await?;
        let current = current_cursor(&mut transaction).await?;
        if cursor > current {
            return Err(AttentionCorruption::Invalid("follow cursor").into());
        }
        let rows = sqlx::query(
            "SELECT change_sequence, session_id
               FROM operator_attention_change
              WHERE change_sequence > $1
              ORDER BY change_sequence
              LIMIT $2",
        )
        .bind(i64::try_from(cursor.value()).map_err(|_| AttentionCorruption::Invalid("cursor"))?)
        .bind(i64::from(max_attention_change_items()) + 1)
        .fetch_all(&mut *transaction)
        .await?;
        if rows.len() > usize::from(max_attention_change_items()) {
            transaction.commit().await?;
            return Ok(AttentionChanges::ResyncRequired { cursor: current });
        }
        let next = rows
            .last()
            .map(|row| {
                row.try_get("change_sequence")
                    .map_err(AttentionRepositoryError::from)
                    .and_then(cursor_from_i64)
            })
            .transpose()?
            .unwrap_or(cursor);
        let identities = rows
            .iter()
            .map(|row| row.try_get::<Uuid, _>("session_id"))
            .collect::<Result<BTreeSet<_>, _>>()?
            .into_iter()
            .collect::<Vec<_>>();
        let summaries = load_summaries(&mut transaction, Some(&identities), None).await?;
        transaction.commit().await?;
        Ok(AttentionChanges::Updated {
            cursor: next,
            summaries,
        })
    }

    async fn read_transaction(
        &self,
    ) -> Result<Transaction<'_, Postgres>, AttentionRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await?;
        Ok(transaction)
    }
}

impl AttentionReader for AttentionRepository {
    type Error = AttentionRepositoryError;

    async fn snapshot(&self, after: Option<SessionId>) -> Result<AttentionSnapshot, Self::Error> {
        AttentionRepository::snapshot(self, after).await
    }

    async fn changes_after(
        &self,
        cursor: AttentionCursor,
    ) -> Result<AttentionChanges, Self::Error> {
        AttentionRepository::changes_after(self, cursor).await
    }
}

async fn current_cursor(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<AttentionCursor, AttentionRepositoryError> {
    let value = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT max(change_sequence) FROM operator_attention_change",
    )
    .fetch_one(&mut **transaction)
    .await?
    .unwrap_or(0);
    cursor_from_i64(value)
}

fn cursor_from_i64(value: i64) -> Result<AttentionCursor, AttentionRepositoryError> {
    let value = u64::try_from(value).map_err(|_| AttentionCorruption::Invalid("cursor"))?;
    Ok(AttentionCursor::new(value))
}

const SUMMARY_SQL: &str = r#"
WITH selected AS (
    SELECT session_id FROM session
     WHERE ($2::uuid[] IS NULL AND ($3::uuid IS NULL OR session_id > $3))
        OR ($2::uuid[] IS NOT NULL AND session_id = ANY($2))
     ORDER BY session_id LIMIT $1
), latest_turn AS (
    SELECT DISTINCT ON (lifecycle.session_id)
           lifecycle.session_id, lifecycle.turn_id, lifecycle.state_kind,
           lifecycle.active_phase_kind, lifecycle.terminal_disposition_kind,
           lifecycle.approval_tool_request_id, current_goal.goal_generation
      FROM turn_lifecycle AS lifecycle JOIN selected USING (session_id)
      LEFT JOIN goal_turn AS current_goal
        ON current_goal.session_id = lifecycle.session_id
       AND current_goal.turn_id = lifecycle.turn_id
     WHERE NOT EXISTS (
               SELECT 1
                 FROM goal_turn_retired_outbox_event AS retired
                WHERE retired.session_id = lifecycle.session_id
                  AND retired.turn_id = lifecycle.turn_id
           )
     ORDER BY lifecycle.session_id,
              CASE lifecycle.state_kind
                  WHEN 'active' THEN 0
                  WHEN 'queued' THEN 1
                  ELSE 2
              END,
              lifecycle.acceptance_position DESC
), latest_goal AS (
    SELECT DISTINCT ON (goal.session_id)
           goal.session_id, goal.generation::text AS generation, goal.event_kind,
           goal.blocked_reason, LEFT(goal.need, $4) AS need_summary
      FROM goal_event AS goal JOIN selected USING (session_id)
     ORDER BY goal.session_id, goal.event_ordinal DESC
), judge AS (
    SELECT call.session_id,
           count(*) FILTER (WHERE call.state_kind <> 'terminal') AS actionable,
           count(*) FILTER (WHERE call.terminal_disposition_kind = 'completed'
                              AND call.recommendation_kind <> 'escalate_to_human') AS completed,
           count(*) FILTER (WHERE call.terminal_disposition_kind = 'completed'
                              AND call.recommendation_kind = 'escalate_to_human') AS escalated,
           count(*) FILTER (WHERE call.state_kind = 'terminal'
                              AND call.terminal_disposition_kind <> 'completed') AS failed
      FROM tool_approval_judge_model_call AS call JOIN selected USING (session_id)
     GROUP BY call.session_id
), latest_runner AS (
    SELECT DISTINCT ON (placement.session_id)
           placement.session_id, placement.state_kind
      FROM runner_session_placement_record AS placement JOIN selected USING (session_id)
     ORDER BY placement.session_id, placement.event_ordinal DESC
), latest_activity AS (
    SELECT DISTINCT ON (change.session_id)
           change.session_id, change.fact_kind, change.recorded_at
      FROM operator_attention_change AS change JOIN selected USING (session_id)
     ORDER BY change.session_id, change.change_sequence DESC
)
SELECT selected.session_id, turn.turn_id, turn.state_kind AS turn_state,
       turn.active_phase_kind, turn.terminal_disposition_kind,
       CASE
           WHEN request.approval_posture = 'human' THEN true
           WHEN request.approval_posture = 'delegated' THEN
               approval_call.state_kind = 'terminal'
               AND (
                   (approval_call.terminal_disposition_kind = 'completed'
                    AND approval_call.recommendation_kind = 'escalate_to_human')
                   OR approval_call.terminal_disposition_kind IN (
                       'known_failed', 'refused', 'cancelled', 'ambiguous'
                   )
               )
               AND (
                   NOT (
                       COALESCE(turn.goal_generation = 1, false)
                       AND (
                           EXISTS (
                               SELECT 1
                                 FROM repo_watch_dispatch_action AS dispatched
                                WHERE dispatched.session_id = selected.session_id
                           )
                           OR EXISTS (
                               SELECT 1
                                 FROM commissioned_dispatch AS dispatched
                                WHERE dispatched.session_id = selected.session_id
                           )
                       )
                   )
                   OR EXISTS (
                       SELECT 1
                         FROM accepted_input AS steering
                        WHERE steering.session_id = selected.session_id
                          AND steering.expected_active_turn_id = turn.turn_id
                          AND steering.disposition_kind = 'pending_steering'
                   )
                   OR EXISTS (
                       SELECT 1
                         FROM repo_watch_headless_approval_escalation AS escalation
                        WHERE escalation.session_id = selected.session_id
                   )
                   OR EXISTS (
                       SELECT 1
                         FROM commissioned_dispatch_headless_approval_escalation AS escalation
                        WHERE escalation.session_id = selected.session_id
                   )
               )
           ELSE false
       END AS approval_human_authority,
       goal.generation, goal.event_kind AS goal_state, goal.blocked_reason, goal.need_summary,
       COALESCE(judge.actionable, 0) AS judge_actionable,
       COALESCE(judge.completed, 0) AS judge_completed,
       COALESCE(judge.escalated, 0) AS judge_escalated,
       COALESCE(judge.failed, 0) AS judge_failed,
       runner.state_kind AS runner_state,
       activity.fact_kind, activity.recorded_at
  FROM selected
  LEFT JOIN latest_turn AS turn
    ON turn.session_id = selected.session_id
  LEFT JOIN tool_request AS request
    ON request.request_id = turn.approval_tool_request_id
  LEFT JOIN tool_approval_judge_model_call AS approval_call
    ON approval_call.request_id = request.request_id
  LEFT JOIN latest_goal AS goal
    ON goal.session_id = selected.session_id
  LEFT JOIN judge
    ON judge.session_id = selected.session_id
  LEFT JOIN latest_runner AS runner
    ON runner.session_id = selected.session_id
  LEFT JOIN latest_activity AS activity
    ON activity.session_id = selected.session_id
 ORDER BY selected.session_id
"#;

async fn load_summaries(
    transaction: &mut Transaction<'_, Postgres>,
    identities: Option<&[Uuid]>,
    after: Option<SessionId>,
) -> Result<Vec<AttentionSummary>, AttentionRepositoryError> {
    if identities.is_some_and(<[Uuid]>::is_empty) {
        return Ok(Vec::new());
    }
    let limit = identities.map_or_else(
        || i64::from(max_attention_snapshot_items()) + 1,
        |values| i64::try_from(values.len()).unwrap_or(i64::MAX),
    );
    sqlx::query(SUMMARY_SQL)
        .bind(limit)
        .bind(identities.map(<[Uuid]>::to_vec))
        .bind(after.map(SessionId::into_uuid))
        .bind(i32::from(max_attention_goal_summary_characters()))
        .fetch_all(&mut **transaction)
        .await?
        .iter()
        .map(decode_summary)
        .collect()
}

fn decode_summary(row: &PgRow) -> Result<AttentionSummary, AttentionRepositoryError> {
    let runner = row.try_get::<Option<String>, _>("runner_state")?;
    let turn_state = row.try_get::<Option<String>, _>("turn_state")?;
    let phase = row.try_get::<Option<String>, _>("active_phase_kind")?;
    let terminal = row.try_get::<Option<String>, _>("terminal_disposition_kind")?;
    let goal_state = row.try_get::<Option<String>, _>("goal_state")?;
    let state = classify_state(
        runner.as_deref(),
        goal_state.as_deref(),
        turn_state.as_deref(),
        phase.as_deref(),
        terminal.as_deref(),
    )?;
    let approval_human_authority = row
        .try_get::<Option<bool>, _>("approval_human_authority")?
        .unwrap_or(false);
    let action = match state {
        AttentionState::Blocked => Some(AttentionAction::ProvideGoalNeed),
        AttentionState::AwaitingApproval if approval_human_authority => {
            Some(AttentionAction::DecideApproval)
        }
        AttentionState::Ambiguous | AttentionState::AwaitingReconciliation => {
            Some(AttentionAction::ReconcileTurn)
        }
        AttentionState::AwaitingApproval
        | AttentionState::AwaitingToolRecovery
        | AttentionState::RunnerLost => None,
        AttentionState::Active | AttentionState::Queued | AttentionState::Idle => None,
    };
    let fact_kind = required_string(row, "fact_kind")?;
    let recorded_at = row
        .try_get::<Option<sqlx::types::time::OffsetDateTime>, _>("recorded_at")?
        .ok_or(AttentionCorruption::Missing("activity timestamp"))?;
    Ok(AttentionSummary {
        session: SessionId::from_uuid(row.try_get("session_id")?),
        current_turn: row
            .try_get::<Option<Uuid>, _>("turn_id")?
            .map(TurnId::from_uuid),
        state,
        action,
        goal_block: decode_goal_block(row, goal_state.as_deref())?,
        judge: AttentionJudgeFacts {
            actionable: nonnegative(row.try_get("judge_actionable")?, "judge actionable")?,
            completed: nonnegative(row.try_get("judge_completed")?, "judge completed")?,
            escalated: nonnegative(row.try_get("judge_escalated")?, "judge escalated")?,
            failed: nonnegative(row.try_get("judge_failed")?, "judge failed")?,
        },
        last_activity: AttentionActivity {
            recorded_at: SystemTime::from(recorded_at),
            kind: decode_activity_kind(&fact_kind)?,
        },
    })
}

fn classify_state(
    runner: Option<&str>,
    goal: Option<&str>,
    turn: Option<&str>,
    phase: Option<&str>,
    terminal: Option<&str>,
) -> Result<AttentionState, AttentionRepositoryError> {
    match runner {
        Some("runner_lost" | "runner_lost_before_pin") => {
            return Ok(AttentionState::RunnerLost);
        }
        None | Some("unpinned" | "pinned" | "runner_abandoned") => {}
        Some(value) => {
            return Err(AttentionCorruption::Unsupported {
                field: "runner state",
                value: value.to_owned(),
            }
            .into());
        }
    }
    if goal == Some("blocked") {
        return Ok(AttentionState::Blocked);
    }
    match (turn, phase, terminal) {
        (Some("active"), Some("awaiting_tool_approval"), _) => Ok(AttentionState::AwaitingApproval),
        (Some("active"), Some("awaiting_model_call_recovery"), _) => Ok(AttentionState::Ambiguous),
        (Some("active"), Some("awaiting_tool_recovery"), _) => {
            Ok(AttentionState::AwaitingToolRecovery)
        }
        (Some("active"), Some("awaiting_runner_recovery"), _) => Ok(AttentionState::RunnerLost),
        (Some("active"), Some("running" | "awaiting_child"), _) => Ok(AttentionState::Active),
        (Some("queued"), None, _) => Ok(AttentionState::Queued),
        (Some("terminal"), None, Some("reconciliation_required")) => {
            Ok(AttentionState::AwaitingReconciliation)
        }
        (Some("terminal"), None, Some(_)) | (None, None, None) => Ok(AttentionState::Idle),
        (Some("active" | "queued" | "terminal"), _, _) => {
            Err(AttentionCorruption::Invalid("turn state shape").into())
        }
        (Some(value), _, _) => Err(AttentionCorruption::Unsupported {
            field: "turn state",
            value: value.to_owned(),
        }
        .into()),
        _ => Err(AttentionCorruption::Invalid("turn state shape").into()),
    }
}

fn decode_goal_block(
    row: &PgRow,
    goal_state: Option<&str>,
) -> Result<Option<AttentionGoalBlock>, AttentionRepositoryError> {
    if goal_state != Some("blocked") {
        return Ok(None);
    }
    let stored_reason = required_string(row, "blocked_reason")?;
    let reason = match goal_blocked_reason_from_str(&stored_reason) {
        Some(GoalBlockedReasonKind::UserInputRequired) => AttentionBlockedReason::UserInputRequired,
        Some(GoalBlockedReasonKind::ExternalChangeRequired) => {
            AttentionBlockedReason::ExternalChangeRequired
        }
        Some(GoalBlockedReasonKind::AuthorizationRequired) => {
            AttentionBlockedReason::AuthorizationRequired
        }
        Some(GoalBlockedReasonKind::ExecutionFailure) => AttentionBlockedReason::ExecutionFailure,
        None => {
            return Err(AttentionCorruption::Unsupported {
                field: "goal blocked reason",
                value: stored_reason,
            }
            .into());
        }
    };
    Ok(Some(AttentionGoalBlock {
        generation: required_string(row, "generation")?
            .parse()
            .map_err(|_| AttentionCorruption::Invalid("goal generation"))?,
        reason,
        need_summary: required_string(row, "need_summary")?,
    }))
}

fn decode_activity_kind(value: &str) -> Result<AttentionActivityKind, AttentionRepositoryError> {
    match value {
        "session" => Ok(AttentionActivityKind::Session),
        "turn" => Ok(AttentionActivityKind::Turn),
        "goal" => Ok(AttentionActivityKind::Goal),
        "approval_judge" => Ok(AttentionActivityKind::ApprovalJudge),
        "runner" => Ok(AttentionActivityKind::Runner),
        _ => Err(AttentionCorruption::Unsupported {
            field: "activity kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn required_string(row: &PgRow, field: &'static str) -> Result<String, AttentionRepositoryError> {
    row.try_get::<Option<String>, _>(field)?
        .ok_or_else(|| AttentionCorruption::Missing(field).into())
}

fn nonnegative(value: i64, field: &'static str) -> Result<u64, AttentionRepositoryError> {
    u64::try_from(value).map_err(|_| AttentionCorruption::Invalid(field).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_precedence_keeps_operator_actions_explicit() {
        assert_eq!(
            classify_state(
                Some("runner_lost"),
                Some("blocked"),
                Some("active"),
                Some("running"),
                None
            )
            .unwrap(),
            AttentionState::RunnerLost
        );
        assert_eq!(
            classify_state(None, Some("blocked"), Some("active"), Some("running"), None).unwrap(),
            AttentionState::Blocked
        );
        assert_eq!(
            classify_state(
                None,
                None,
                Some("active"),
                Some("awaiting_tool_approval"),
                None
            )
            .unwrap(),
            AttentionState::AwaitingApproval
        );
    }

    #[test]
    fn tool_recovery_is_distinct_from_model_recovery() {
        assert_eq!(
            classify_state(
                None,
                None,
                Some("active"),
                Some("awaiting_tool_recovery"),
                None
            )
            .unwrap(),
            AttentionState::AwaitingToolRecovery
        );
    }

    #[test]
    fn runner_placement_vocabulary_has_deliberate_attention_semantics() {
        assert_eq!(
            classify_state(
                Some("runner_lost_before_pin"),
                None,
                Some("queued"),
                None,
                None
            )
            .unwrap(),
            AttentionState::RunnerLost
        );
        assert_eq!(
            classify_state(
                Some("runner_lost"),
                Some("blocked"),
                Some("active"),
                Some("running"),
                None
            )
            .unwrap(),
            AttentionState::RunnerLost
        );
        assert_eq!(
            classify_state(
                Some("unpinned"),
                Some("blocked"),
                Some("active"),
                Some("running"),
                None
            )
            .unwrap(),
            AttentionState::Blocked
        );
        assert_eq!(
            classify_state(Some("pinned"), None, Some("queued"), None, None).unwrap(),
            AttentionState::Queued
        );
        assert_eq!(
            classify_state(Some("runner_abandoned"), None, None, None, None).unwrap(),
            AttentionState::Idle
        );

        let error = classify_state(Some("future_runner_state"), None, None, None, None)
            .expect_err("unknown runner placement states must fail closed");
        assert!(matches!(
            error,
            AttentionRepositoryError::Corruption(AttentionCorruption::Unsupported {
                field: "runner state",
                ..
            })
        ));
    }

    #[test]
    fn supported_turn_with_invalid_shape_reports_shape_corruption() {
        let error = classify_state(None, None, Some("active"), None, None)
            .expect_err("a supported state with a missing phase is corrupt");

        assert!(matches!(
            error,
            AttentionRepositoryError::Corruption(AttentionCorruption::Invalid("turn state shape"))
        ));
    }
}
