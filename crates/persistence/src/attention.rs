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

use crate::{
    mapping::{
        GoalEventDiscriminator, dispatched_runner_state_from_str, goal_blocked_reason_from_str,
        goal_event_kind_from_str,
    },
    outbox::DispatchedRunnerState,
};

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
        verify_fact_completeness(&mut transaction).await?;
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
            "SELECT change_sequence, session_id, fact_kind
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
        let membership_changed = rows
            .iter()
            .map(|row| row.try_get::<String, _>("fact_kind"))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|fact_kind| fact_kind == "session");
        if membership_changed {
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
            r#")
SELECT selected.session_id, selected.attention_turn_id AS turn_id,
       selected.attention_turn_state_kind AS turn_state,
       selected.attention_turn_active_phase_kind AS active_phase_kind,
       selected.attention_turn_terminal_disposition_kind AS terminal_disposition_kind,
       selected.title_summary, selected.title_truncated, selected.archived,
       selected.active_turn_count, selected.queued_turn_count,
       goal.generation, goal.event_kind AS goal_state, goal.blocked_reason, goal.need_summary,
       selected.judge_actionable, selected.judge_completed,
       selected.judge_escalated, selected.judge_failed,
       runner.state_kind AS runner_state,
       selected.fact_kind, selected.recorded_at
  FROM selected
  LEFT JOIN LATERAL (
      SELECT event.generation::text AS generation, event.event_kind,
             event.blocked_reason, LEFT(event.need, $4) AS need_summary
        FROM goal_event AS event
       WHERE event.session_id = selected.session_id
       ORDER BY event.event_ordinal DESC
       LIMIT 1
  ) AS goal ON true
  LEFT JOIN LATERAL (
      SELECT placement.state_kind
        FROM runner_session_placement_record AS placement
       WHERE placement.session_id = selected.session_id
       ORDER BY placement.event_ordinal DESC
       LIMIT 1
  ) AS runner ON true "#,
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
           facts.attention_turn_id,
           facts.attention_turn_state_kind,
           facts.attention_turn_active_phase_kind,
           facts.attention_turn_terminal_disposition_kind,
           facts.attention_activity_kind AS fact_kind,
           facts.attention_activity_recorded_at AS recorded_at
      FROM session AS session_row
      LEFT JOIN session_metadata AS metadata USING (session_id)
      LEFT JOIN session_timeline_fact AS facts USING (session_id)
     WHERE ($2::uuid[] IS NOT NULL
            AND session_row.session_id = ANY($2)
            AND NOT COALESCE(metadata.archived, false))
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

// The page scan is driven by the indexed activity facts so the keyset order
// stays a bounded ordered scan; completeness against `session` (which drives
// the count query) is enforced separately by `verify_fact_completeness`, so a
// session whose `session_timeline_fact` row is missing fails the snapshot
// closed instead of being silently omitted while the exact total counts it.
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
           facts.attention_turn_id,
           facts.attention_turn_state_kind,
           facts.attention_turn_active_phase_kind,
           facts.attention_turn_terminal_disposition_kind,
           facts.attention_activity_kind AS fact_kind,
           facts.attention_activity_recorded_at AS recorded_at
      FROM session_timeline_fact AS facts
      JOIN session AS session_row USING (session_id)
      LEFT JOIN session_metadata AS metadata USING (session_id)
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
            OR facts.attention_activity_recorded_at < $9
            OR (facts.attention_activity_recorded_at = $9
                AND session_row.session_id > $3))
     ORDER BY facts.attention_activity_recorded_at DESC, session_row.session_id LIMIT $1
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

/// Fails the coherent read closed when any durable session is missing its
/// `session_timeline_fact` row. The activity-ordered page scan is driven by
/// the fact table for its index, while the exact total is counted from
/// `session`; without this probe a missing projection row would silently
/// shrink pages while the total still counted the session. The probe is a
/// primary-key anti-join with an immediate limit, so it costs no more than
/// the exact count that already runs in the same transaction.
async fn verify_fact_completeness(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), AttentionRepositoryError> {
    let missing = sqlx::query_scalar::<_, i32>(
        "SELECT 1
           FROM session
           LEFT JOIN session_timeline_fact USING (session_id)
          WHERE session_timeline_fact.session_id IS NULL
          LIMIT 1",
    )
    .fetch_optional(&mut **transaction)
    .await?;
    if missing.is_some() {
        return Err(AttentionCorruption::Missing("session activity fact").into());
    }
    Ok(())
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
    let goal_state = row
        .try_get::<Option<String>, _>("goal_state")?
        .map(|value| decode_goal_event_kind(&value))
        .transpose()?;
    let state = classify_state(
        runner.as_deref(),
        goal_state,
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
    let goal_block = if state == AttentionState::Blocked {
        decode_goal_block(row, goal_state)?
    } else {
        None
    };
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
        goal_block,
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
    goal: Option<GoalEventDiscriminator>,
    turn: Option<&str>,
    phase: Option<&str>,
    terminal: Option<&str>,
) -> Result<AttentionState, AttentionRepositoryError> {
    // Every stored fact is validated before precedence selects a winner, so
    // corruption in a lower-precedence fact still fails closed instead of
    // hiding behind runner loss or a blocked goal.
    let runner_state = runner
        .map(|value| {
            dispatched_runner_state_from_str(value).ok_or(AttentionCorruption::Unsupported {
                field: "runner state",
                value: value.to_owned(),
            })
        })
        .transpose()?;
    let turn_state = classify_turn_shape(turn, phase, terminal)?;
    if matches!(
        runner_state,
        Some(DispatchedRunnerState::RunnerLost | DispatchedRunnerState::RunnerLostBeforePin)
    ) {
        return Ok(AttentionState::RunnerLost);
    }
    if goal == Some(GoalEventDiscriminator::Blocked) {
        return Ok(AttentionState::Blocked);
    }
    Ok(turn_state)
}

fn classify_turn_shape(
    turn: Option<&str>,
    phase: Option<&str>,
    terminal: Option<&str>,
) -> Result<AttentionState, AttentionRepositoryError> {
    match (turn, phase, terminal) {
        (Some("active"), Some("awaiting_tool_approval"), None) => {
            Ok(AttentionState::AwaitingApproval)
        }
        (Some("active"), Some("awaiting_model_call_recovery" | "awaiting_tool_recovery"), None) => {
            Ok(AttentionState::Ambiguous)
        }
        (Some("active"), Some("awaiting_runner_recovery"), None) => Ok(AttentionState::RunnerLost),
        (Some("active"), Some("running" | "awaiting_child"), None) => Ok(AttentionState::Active),
        (Some("queued"), None, None) => Ok(AttentionState::Queued),
        (Some("terminal"), None, Some("reconciliation_required")) => {
            Ok(AttentionState::AwaitingReconciliation)
        }
        (Some("terminal"), None, Some("completed" | "refused" | "failed" | "cancelled"))
        | (None, None, None) => Ok(AttentionState::Idle),
        (Some("terminal"), None, Some(value)) => Err(AttentionCorruption::Unsupported {
            field: "turn terminal disposition",
            value: value.to_owned(),
        }
        .into()),
        (Some("active" | "queued"), _, Some(_)) => {
            Err(AttentionCorruption::Invalid("nonterminal turn disposition shape").into())
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
    goal_state: Option<GoalEventDiscriminator>,
) -> Result<Option<AttentionGoalBlock>, AttentionRepositoryError> {
    if goal_state != Some(GoalEventDiscriminator::Blocked) {
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

fn decode_goal_event_kind(value: &str) -> Result<GoalEventDiscriminator, AttentionRepositoryError> {
    goal_event_kind_from_str(value).ok_or_else(|| {
        AttentionCorruption::Unsupported {
            field: "goal event kind",
            value: value.to_owned(),
        }
        .into()
    })
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
                Some(GoalEventDiscriminator::Blocked),
                Some("active"),
                Some("running"),
                None
            )
            .unwrap(),
            AttentionState::RunnerLost
        );
        assert_eq!(
            classify_state(
                None,
                Some(GoalEventDiscriminator::Blocked),
                Some("active"),
                Some("running"),
                None,
            )
            .unwrap(),
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
    fn goal_event_kind_decoding_rejects_unknown_storage_values() {
        assert!(decode_goal_event_kind("future_goal_state").is_err());
    }

    #[test]
    fn state_classification_rejects_a_terminal_disposition_on_an_active_turn() {
        assert_eq!(
            classify_state(
                None,
                None,
                Some("active"),
                Some("running"),
                Some("completed"),
            )
            .unwrap_err()
            .to_string(),
            "invalid operator attention nonterminal turn disposition shape"
        );
    }

    #[test]
    fn state_classification_rejects_a_terminal_disposition_on_a_queued_turn() {
        assert_eq!(
            classify_state(None, None, Some("queued"), None, Some("cancelled"))
                .unwrap_err()
                .to_string(),
            "invalid operator attention nonterminal turn disposition shape"
        );
    }

    #[test]
    fn state_classification_validates_turn_facts_before_runner_loss_precedence() {
        assert_eq!(
            classify_state(
                Some("runner_lost"),
                None,
                Some("terminal"),
                None,
                Some("future_disposition"),
            )
            .unwrap_err()
            .to_string(),
            "unsupported operator attention turn terminal disposition: future_disposition"
        );
    }

    #[test]
    fn state_classification_validates_turn_facts_before_blocked_goal_precedence() {
        assert_eq!(
            classify_state(
                None,
                Some(GoalEventDiscriminator::Blocked),
                Some("future_turn_state"),
                None,
                None,
            )
            .unwrap_err()
            .to_string(),
            "unsupported operator attention turn state: future_turn_state"
        );
    }

    #[test]
    fn state_classification_rejects_unknown_terminal_disposition_spellings() {
        assert_eq!(
            classify_state(
                None,
                None,
                Some("terminal"),
                None,
                Some("future_disposition"),
            )
            .unwrap_err()
            .to_string(),
            "unsupported operator attention turn terminal disposition: future_disposition"
        );
    }

    #[test]
    fn state_classification_rejects_unknown_runner_state_spellings() {
        assert_eq!(
            classify_state(
                Some("future_runner_state"),
                None,
                Some("active"),
                Some("running"),
                None,
            )
            .unwrap_err()
            .to_string(),
            "unsupported operator attention runner state: future_runner_state"
        );
    }

    #[test]
    fn state_classification_treats_known_healthy_runner_states_as_placements() {
        assert_eq!(
            classify_state(Some("suspect"), None, Some("active"), Some("running"), None).unwrap(),
            AttentionState::Active
        );
    }
}
