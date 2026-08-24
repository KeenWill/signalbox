//! PostgreSQL adapter for coherent bounded fleet attention reads.

use std::{collections::BTreeSet, error::Error, fmt, time::SystemTime};

use signalbox_application::{
    AttentionAction, AttentionActivity, AttentionActivityKind, AttentionBlockedReason,
    AttentionChanges, AttentionContinuation, AttentionCursor, AttentionGoalBlock,
    AttentionJudgeFacts, AttentionQuery, AttentionReader, AttentionSnapshot, AttentionSort,
    AttentionState, AttentionSummary, max_attention_change_items,
    max_attention_goal_summary_characters, max_attention_snapshot_items,
    max_attention_title_characters,
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
        query: AttentionQuery,
    ) -> Result<AttentionSnapshot, AttentionRepositoryError> {
        let mut transaction = self.read_transaction().await?;
        let cursor = current_cursor(&mut transaction).await?;
        let total = count_catalog_matches(&mut transaction, &query).await?;
        let mut summaries = load_summaries(&mut transaction, None, Some(&query)).await?;
        let has_more = summaries.len() > usize::from(max_attention_snapshot_items());
        summaries.truncate(usize::from(max_attention_snapshot_items()));
        let continuation = has_more
            .then(|| {
                summaries
                    .last()
                    .map(|row| continuation_for(row, query.sort()))
            })
            .flatten();
        transaction.commit().await?;
        Ok(AttentionSnapshot {
            cursor,
            total,
            sort: query.sort(),
            summaries,
            continuation,
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

    async fn snapshot(&self, query: AttentionQuery) -> Result<AttentionSnapshot, Self::Error> {
        AttentionRepository::snapshot(self, query).await
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

macro_rules! summary_sql {
    ($selection:literal, $ordering:literal) => {
        concat!(
            "WITH selected AS (",
            $selection,
            r#"), latest_turn AS (
    SELECT DISTINCT ON (lifecycle.session_id)
           lifecycle.session_id, lifecycle.turn_id, lifecycle.state_kind,
           lifecycle.active_phase_kind, lifecycle.terminal_disposition_kind
      FROM turn_lifecycle AS lifecycle JOIN selected USING (session_id)
     WHERE NOT lifecycle.delegation_runtime_terminal
       AND goal_turn_is_runtime_relevant(lifecycle.session_id, lifecycle.turn_id)
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
), latest_runner AS (
    SELECT DISTINCT ON (placement.session_id)
           placement.session_id, placement.state_kind
      FROM runner_session_placement_record AS placement JOIN selected USING (session_id)
     ORDER BY placement.session_id, placement.event_ordinal DESC
)
SELECT selected.session_id, turn.turn_id, turn.state_kind AS turn_state,
       turn.active_phase_kind, turn.terminal_disposition_kind,
       selected.title_summary, selected.title_truncated, selected.archived,
       selected.active_turn_count, selected.queued_turn_count,
       goal.generation, goal.event_kind AS goal_state, goal.blocked_reason, goal.need_summary,
       selected.judge_actionable, selected.judge_completed,
       selected.judge_escalated, selected.judge_failed,
       runner.state_kind AS runner_state,
       selected.fact_kind, selected.recorded_at
  FROM selected
  LEFT JOIN latest_turn AS turn USING (session_id)
  LEFT JOIN latest_goal AS goal USING (session_id)
  LEFT JOIN latest_runner AS runner USING (session_id) "#,
            $ordering
        )
    };
}

const SELECT_IDENTITY: &str = summary_sql!(
    r#"
    SELECT session_row.session_id,
           LEFT(metadata.title, $5) AS title_summary,
           metadata.title IS NOT NULL AND length(metadata.title) > $5 AS title_truncated,
           COALESCE(metadata.archived, false) AS archived,
           facts.active_turn_count::text AS active_turn_count,
           facts.queued_turn_count::text AS queued_turn_count,
           facts.approval_judge_actionable_count::text AS judge_actionable,
           facts.approval_judge_completed_count::text AS judge_completed,
           facts.approval_judge_escalated_count::text AS judge_escalated,
           facts.approval_judge_failed_count::text AS judge_failed,
           activity.fact_kind, activity.recorded_at
      FROM session AS session_row
      LEFT JOIN session_metadata AS metadata USING (session_id)
      LEFT JOIN session_timeline_fact AS facts USING (session_id)
      LEFT JOIN LATERAL (
          SELECT change.fact_kind, change.recorded_at
            FROM operator_attention_change AS change
           WHERE change.session_id = session_row.session_id
           ORDER BY change.change_sequence DESC LIMIT 1
      ) AS activity ON true
     WHERE ($2::uuid[] IS NOT NULL AND session_row.session_id = ANY($2))
        OR ($2::uuid[] IS NULL
            AND ($3::uuid IS NULL OR session_row.session_id > $3)
            AND ($6::text IS NULL
                 OR strpos(COALESCE(metadata.title, ''), $6) > 0
                 OR strpos(session_row.session_id::text, $6) > 0)
            AND ($8 OR NOT COALESCE(metadata.archived, false))
            AND NOT EXISTS (
                SELECT 1 FROM unnest($7::text[]) AS required(tag)
                 WHERE NOT EXISTS (
                    SELECT 1 FROM session_metadata_tag AS stored
                     WHERE stored.session_id = session_row.session_id
                       AND stored.tag = required.tag)))
            AND $9::timestamptz IS NULL
     ORDER BY session_row.session_id LIMIT $1
    "#,
    "ORDER BY selected.session_id"
);

const SELECT_LAST_ACTIVITY: &str = summary_sql!(
    r#"
    SELECT session_row.session_id,
           LEFT(metadata.title, $5) AS title_summary,
           metadata.title IS NOT NULL AND length(metadata.title) > $5 AS title_truncated,
           COALESCE(metadata.archived, false) AS archived,
           facts.active_turn_count::text AS active_turn_count,
           facts.queued_turn_count::text AS queued_turn_count,
           facts.approval_judge_actionable_count::text AS judge_actionable,
           facts.approval_judge_completed_count::text AS judge_completed,
           facts.approval_judge_escalated_count::text AS judge_escalated,
           facts.approval_judge_failed_count::text AS judge_failed,
           activity.fact_kind, activity.recorded_at
      FROM session AS session_row
      LEFT JOIN session_metadata AS metadata USING (session_id)
      LEFT JOIN session_timeline_fact AS facts USING (session_id)
      LEFT JOIN LATERAL (
          SELECT change.fact_kind, change.recorded_at
            FROM operator_attention_change AS change
           WHERE change.session_id = session_row.session_id
           ORDER BY change.change_sequence DESC LIMIT 1
      ) AS activity ON true
     WHERE ($6::text IS NULL
            OR strpos(COALESCE(metadata.title, ''), $6) > 0
            OR strpos(session_row.session_id::text, $6) > 0)
       AND ($8 OR NOT COALESCE(metadata.archived, false))
       AND NOT EXISTS (
           SELECT 1 FROM unnest($7::text[]) AS required(tag)
            WHERE NOT EXISTS (
               SELECT 1 FROM session_metadata_tag AS stored
                WHERE stored.session_id = session_row.session_id
                  AND stored.tag = required.tag))
       AND ($9::timestamptz IS NULL
            OR activity.recorded_at < $9
            OR (activity.recorded_at = $9 AND session_row.session_id > $3))
     ORDER BY activity.recorded_at DESC, session_row.session_id LIMIT $1
    "#,
    "ORDER BY selected.recorded_at DESC, selected.session_id"
);

const COUNT_CATALOG_MATCHES_SQL: &str = r#"
SELECT count(*)
  FROM session AS session_row
  LEFT JOIN session_metadata AS metadata USING (session_id)
 WHERE ($1::text IS NULL
        OR strpos(COALESCE(metadata.title, ''), $1) > 0
        OR strpos(session_row.session_id::text, $1) > 0)
   AND ($3 OR NOT COALESCE(metadata.archived, false))
   AND NOT EXISTS (
       SELECT 1 FROM unnest($2::text[]) AS required(tag)
        WHERE NOT EXISTS (
           SELECT 1 FROM session_metadata_tag AS stored
            WHERE stored.session_id = session_row.session_id
              AND stored.tag = required.tag))
"#;

async fn load_summaries(
    transaction: &mut Transaction<'_, Postgres>,
    identities: Option<&[Uuid]>,
    query: Option<&AttentionQuery>,
) -> Result<Vec<AttentionSummary>, AttentionRepositoryError> {
    if identities.is_some_and(<[Uuid]>::is_empty) {
        return Ok(Vec::new());
    }
    let limit = identities.map_or_else(
        || i64::from(max_attention_snapshot_items()) + 1,
        |values| i64::try_from(values.len()).unwrap_or(i64::MAX),
    );
    let (sql, after_session, after_activity) = match query {
        Some(query) => match (query.sort(), query.continuation()) {
            (
                AttentionSort::LastActivityDescending,
                Some(AttentionContinuation::LastActivity {
                    recorded_at,
                    session,
                }),
            ) => (
                SELECT_LAST_ACTIVITY,
                Some(session.into_uuid()),
                Some(offset_date_time_from_system_time(*recorded_at)?),
            ),
            (AttentionSort::LastActivityDescending, None) => (SELECT_LAST_ACTIVITY, None, None),
            (
                AttentionSort::SessionIdentityAscending,
                Some(AttentionContinuation::SessionIdentity(session)),
            ) => (SELECT_IDENTITY, Some(session.into_uuid()), None),
            (AttentionSort::SessionIdentityAscending, None) => (SELECT_IDENTITY, None, None),
            _ => return Err(AttentionCorruption::Invalid("catalog continuation").into()),
        },
        None => (SELECT_IDENTITY, None, None),
    };
    let search = query.and_then(AttentionQuery::search);
    let required_tags: Vec<String> = query
        .map(|query| query.required_tags().map(str::to_owned).collect())
        .unwrap_or_default();
    let include_archived = query.is_some_and(AttentionQuery::include_archived);
    sqlx::query(sql)
        .bind(limit)
        .bind(identities.map(<[Uuid]>::to_vec))
        .bind(after_session)
        .bind(i32::from(max_attention_goal_summary_characters()))
        .bind(i32::from(max_attention_title_characters()))
        .bind(search)
        .bind(required_tags)
        .bind(include_archived)
        .bind(after_activity)
        .fetch_all(&mut **transaction)
        .await?
        .iter()
        .map(decode_summary)
        .collect()
}

fn offset_date_time_from_system_time(
    recorded_at: SystemTime,
) -> Result<sqlx::types::time::OffsetDateTime, AttentionRepositoryError> {
    let nanoseconds = recorded_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| AttentionCorruption::Invalid("catalog continuation timestamp"))?
        .as_nanos();
    let nanoseconds = i128::try_from(nanoseconds)
        .map_err(|_| AttentionCorruption::Invalid("catalog continuation timestamp"))?;
    sqlx::types::time::OffsetDateTime::from_unix_timestamp_nanos(nanoseconds)
        .map_err(|_| AttentionCorruption::Invalid("catalog continuation timestamp").into())
}

async fn count_catalog_matches(
    transaction: &mut Transaction<'_, Postgres>,
    query: &AttentionQuery,
) -> Result<u64, AttentionRepositoryError> {
    let tags: Vec<String> = query.required_tags().map(str::to_owned).collect();
    let count: i64 = sqlx::query_scalar(COUNT_CATALOG_MATCHES_SQL)
        .bind(query.search())
        .bind(tags)
        .bind(query.include_archived())
        .fetch_one(&mut **transaction)
        .await?;
    nonnegative(count, "catalog total")
}

fn continuation_for(summary: &AttentionSummary, sort: AttentionSort) -> AttentionContinuation {
    match sort {
        AttentionSort::LastActivityDescending => AttentionContinuation::LastActivity {
            recorded_at: summary.last_activity.recorded_at,
            session: summary.session,
        },
        AttentionSort::SessionIdentityAscending => {
            AttentionContinuation::SessionIdentity(summary.session)
        }
    }
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
    let action = match state {
        AttentionState::Blocked => Some(AttentionAction::ProvideGoalNeed),
        AttentionState::AwaitingApproval => Some(AttentionAction::DecideApproval),
        AttentionState::Ambiguous | AttentionState::AwaitingReconciliation => {
            Some(AttentionAction::ReconcileTurn)
        }
        AttentionState::RunnerLost => Some(AttentionAction::RestoreRunner),
        AttentionState::Active | AttentionState::Queued | AttentionState::Idle => None,
    };
    let fact_kind = required_string(row, "fact_kind")?;
    let recorded_at = row
        .try_get::<Option<sqlx::types::time::OffsetDateTime>, _>("recorded_at")?
        .ok_or(AttentionCorruption::Missing("activity timestamp"))?;
    Ok(AttentionSummary {
        session: SessionId::from_uuid(row.try_get("session_id")?),
        title_summary: row.try_get("title_summary")?,
        title_truncated: row.try_get("title_truncated")?,
        archived: row.try_get("archived")?,
        current_turn: row
            .try_get::<Option<Uuid>, _>("turn_id")?
            .map(TurnId::from_uuid),
        active_turn_count: required_string(row, "active_turn_count")?
            .parse()
            .map_err(|_| AttentionCorruption::Invalid("active turn count"))?,
        queued_turn_count: required_string(row, "queued_turn_count")?
            .parse()
            .map_err(|_| AttentionCorruption::Invalid("queued turn count"))?,
        state,
        action,
        goal_block: decode_goal_block(row, goal_state.as_deref())?,
        judge: AttentionJudgeFacts {
            actionable: parse_u64(row, "judge_actionable")?,
            completed: parse_u64(row, "judge_completed")?,
            escalated: parse_u64(row, "judge_escalated")?,
            failed: parse_u64(row, "judge_failed")?,
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
    if matches!(runner, Some("runner_lost" | "runner_lost_before_pin")) {
        return Ok(AttentionState::RunnerLost);
    }
    if goal == Some("blocked") {
        return Ok(AttentionState::Blocked);
    }
    match (turn, phase, terminal) {
        (Some("active"), Some("awaiting_tool_approval"), _) => Ok(AttentionState::AwaitingApproval),
        (Some("active"), Some("awaiting_model_call_recovery" | "awaiting_tool_recovery"), _) => {
            Ok(AttentionState::Ambiguous)
        }
        (Some("active"), Some("awaiting_runner_recovery"), _) => Ok(AttentionState::RunnerLost),
        (Some("active"), Some("running" | "awaiting_child"), _) => Ok(AttentionState::Active),
        (Some("queued"), None, _) => Ok(AttentionState::Queued),
        (Some("terminal"), None, Some("reconciliation_required")) => {
            Ok(AttentionState::AwaitingReconciliation)
        }
        (Some("terminal"), None, Some(_)) | (None, None, None) => Ok(AttentionState::Idle),
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

fn parse_u64(row: &PgRow, field: &'static str) -> Result<u64, AttentionRepositoryError> {
    required_string(row, field)?
        .parse()
        .map_err(|_| AttentionCorruption::Invalid(field).into())
}

fn nonnegative(value: i64, field: &'static str) -> Result<u64, AttentionRepositoryError> {
    u64::try_from(value).map_err(|_| AttentionCorruption::Invalid(field).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn continuation_timestamp_conversion_rejects_values_outside_database_range() {
        let beyond_offset_date_time = SystemTime::UNIX_EPOCH + Duration::from_secs(253_402_300_800);
        let before_unix_epoch = SystemTime::UNIX_EPOCH - Duration::from_secs(1);

        assert!(offset_date_time_from_system_time(beyond_offset_date_time).is_err());
        assert!(offset_date_time_from_system_time(before_unix_epoch).is_err());
    }

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
}
