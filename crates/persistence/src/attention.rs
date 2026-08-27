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

/// One bounded attention page without the catalog-only exact total.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionPage {
    pub cursor: AttentionCursor,
    pub sort: AttentionSort,
    pub summaries: Vec<AttentionSummary>,
    pub continuation: Option<AttentionContinuation>,
}

/// The automatic-resume attempt limits an operator projection reads.
///
/// Both must be the numbers the daemon's resume planner applies
/// (`automatic_resume_attempt_budget` and `automatic_resume_attempt_ceiling`);
/// reading different ones makes a projection report a session as needing its
/// operator while the daemon still owes it resumes, or the reverse. `None` is
/// the configured unbounded policy for that limit, under which automatic
/// resumption never ends for that reason. The planner ends a run at whichever
/// limit it reaches first: the budget counts only chargeable failures, so a run
/// whose failures are all exempt ends at the ceiling and at nothing else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutomaticResumeAttemptBounds {
    budget: Option<u32>,
    ceiling: Option<u32>,
}

impl AutomaticResumeAttemptBounds {
    /// Binds both configured limits.
    #[must_use]
    pub const fn new(budget: Option<u32>, ceiling: Option<u32>) -> Self {
        Self { budget, ceiling }
    }

    /// The configured policy under which no automatic-resume run ever ends.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            budget: None,
            ceiling: None,
        }
    }
}

/// Read-only PostgreSQL implementation of the fleet projection port.
#[derive(Clone, Debug)]
pub struct AttentionRepository {
    pool: PgPool,
    automatic_resume_attempts: AutomaticResumeAttemptBounds,
}

impl AttentionRepository {
    /// Binds the projection to the deployment's automatic-resume attempt
    /// limits.
    #[must_use]
    pub const fn new(
        pool: PgPool,
        automatic_resume_attempts: AutomaticResumeAttemptBounds,
    ) -> Self {
        Self {
            pool,
            automatic_resume_attempts,
        }
    }

    pub async fn snapshot(
        &self,
        query: AttentionQuery,
    ) -> Result<AttentionSnapshot, AttentionRepositoryError> {
        let (page, total) = self.read_page(query, true).await?;
        Ok(AttentionSnapshot {
            cursor: page.cursor,
            total,
            sort: page.sort,
            summaries: page.summaries,
            continuation: page.continuation,
        })
    }

    /// Reads the bounded legacy attention projection without scanning the
    /// fleet for the catalog's exact filtered total.
    pub async fn page(
        &self,
        query: AttentionQuery,
    ) -> Result<AttentionPage, AttentionRepositoryError> {
        self.read_page(query, false)
            .await
            .map(|(page, _total)| page)
    }

    async fn read_page(
        &self,
        query: AttentionQuery,
        include_total: bool,
    ) -> Result<(AttentionPage, u64), AttentionRepositoryError> {
        let mut transaction = self.read_transaction().await?;
        let cursor = current_cursor(&mut transaction).await?;
        let total = if include_total {
            verify_fact_completeness(&mut transaction).await?;
            count_catalog_matches(&mut transaction, &query).await?
        } else {
            0
        };
        let mut summaries = load_summaries(
            &mut transaction,
            None,
            Some(&query),
            self.automatic_resume_attempts,
        )
        .await?;
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
        Ok((
            AttentionPage {
                cursor,
                sort: query.sort(),
                summaries,
                continuation,
            },
            total,
        ))
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
        let summaries = load_summaries(
            &mut transaction,
            Some(&identities),
            None,
            self.automatic_resume_attempts,
        )
        .await?;
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
            "WITH RECURSIVE selected AS (",
            $selection,
            r#"), latest_turn AS (
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
       AND NOT lifecycle.delegation_runtime_terminal
       AND (
           lifecycle.state_kind <> 'queued'
           OR accepted_input_turn_is_first_nonterminal(
               lifecycle.session_id, lifecycle.turn_id
           )
       )
     ORDER BY lifecycle.session_id,
              CASE lifecycle.state_kind
                  WHEN 'active' THEN 0
                  WHEN 'queued' THEN 1
                  ELSE 2
              END,
              CASE WHEN lifecycle.state_kind = 'queued'
                   THEN lifecycle.acceptance_position
              END,
              lifecycle.acceptance_position DESC
), latest_goal AS (
    SELECT DISTINCT ON (goal.session_id)
           goal.session_id, goal.event_ordinal,
           goal.generation::text AS generation, goal.event_kind,
           goal.blocked_reason, goal.scheduler_turn_id,
           LEFT(goal.need, $4) AS need_summary
      FROM goal_event AS goal JOIN selected USING (session_id)
     ORDER BY goal.session_id, goal.event_ordinal DESC
), automatic_resume_lineage AS (
    SELECT goal.session_id, goal.generation, goal.event_ordinal AS head_ordinal,
           goal.scheduler_turn_id AS failed_turn_id, 0::integer AS spent,
           0::integer AS attempted
      FROM latest_goal AS goal
     WHERE goal.event_kind = 'blocked'
       AND goal.blocked_reason = 'execution_failure'
       -- A headless approval escalation blocks the goal without arming any
       -- automatic resumption: it writes its `execution_failure` block outside
       -- `PostgresGoalPassDisposition` precisely so that only an operator can
       -- resume the session (`docs/spec/goal-mode.md`). Seeding the lineage
       -- from such a block would report a resumption as pending forever and
       -- suppress `ProvideGoalNeed`, hiding the one session that actually
       -- needs the operator. The block names its scheduler turn, which is the
       -- turn both escalation tables record.
       AND NOT EXISTS (
           SELECT 1
             FROM repo_watch_headless_approval_escalation AS escalation
            WHERE escalation.session_id = goal.session_id
              AND escalation.turn_id = goal.scheduler_turn_id
       )
       AND NOT EXISTS (
           SELECT 1
             FROM commissioned_dispatch_headless_approval_escalation AS escalation
            WHERE escalation.session_id = goal.session_id
              AND escalation.turn_id = goal.scheduler_turn_id
       )
       -- A durable execution-failure recovery cause is the second operator-only
       -- block shape. `PostgresGoalPassDisposition::block_execution_failure`
       -- reads this record before it plans anything and parks the block as
       -- `AutomaticResumption::OperatorRequired`, arming no resume at all
       -- (`docs/spec/goal-mode.md`). Seeding from it suppresses
       -- `ProvideGoalNeed` forever exactly as a headless escalation would. The
       -- record is keyed by the same scheduler turn the block names.
       AND NOT EXISTS (
           SELECT 1
             FROM goal_execution_failure_recovery AS recovery
            WHERE recovery.session_id = goal.session_id
              AND recovery.turn_id = goal.scheduler_turn_id
       )
    UNION ALL
    -- Each step charges the attempt that answered the newer block, which is the
    -- failed turn that block names, and carries the older block's turn forward
    -- for the next step to classify. The predicate is the exact classification
    -- `GoalRepository::unchargeable_automatic_resume_turns` applies before the
    -- resume planner spends the attempt budget: a failure the daemon itself
    -- owns is not charged to the operator's budget. It probes the one named
    -- turn rather than the session's history, so charging costs no more than
    -- the lineage walk it rides on. `attempted` counts every step regardless,
    -- which is the count the planner tests against the lifetime ceiling: a run
    -- of exempt failures charges nothing and so ends at that limit alone.
    SELECT lineage.session_id, lineage.generation, blocked.event_ordinal,
           blocked.scheduler_turn_id,
           lineage.spent + CASE WHEN EXISTS (
               SELECT 1
                 FROM turn_lifecycle AS lifecycle
                 LEFT JOIN automatic_reconciliation AS recovery
                   ON recovery.turn_id = lifecycle.turn_id
                  AND recovery.session_id = lifecycle.session_id
                 LEFT JOIN model_call AS terminal_call
                   ON terminal_call.model_call_id = lifecycle.terminal_model_call_id
                  AND terminal_call.turn_id = lifecycle.turn_id
                  AND terminal_call.session_id = lifecycle.session_id
                 LEFT JOIN tool_continuation_context_headroom AS headroom
                   ON headroom.terminal_attempt_id = lifecycle.terminal_attempt_id
                  AND headroom.turn_id = lifecycle.turn_id
                  AND headroom.session_id = lifecycle.session_id
                WHERE lifecycle.session_id = lineage.session_id
                  AND lifecycle.turn_id = lineage.failed_turn_id
                  AND (recovery.state_kind = 'reconciled'
                       OR headroom.terminal_attempt_id IS NOT NULL
                       OR terminal_call.terminal_provider_failure_cause IN
                          ('rate_limited', 'overloaded', 'provider_internal'))
           ) THEN 0 ELSE 1 END,
           lineage.attempted + 1
      FROM automatic_resume_lineage AS lineage
      JOIN goal_event AS resumed
        ON resumed.session_id = lineage.session_id
       AND resumed.generation::text = lineage.generation
       AND resumed.event_ordinal = lineage.head_ordinal - 1
       AND resumed.event_kind = 'resumed'
      JOIN goal_event AS blocked
        ON blocked.session_id = lineage.session_id
       AND blocked.generation::text = lineage.generation
       AND blocked.event_ordinal = lineage.head_ordinal - 2
       AND blocked.event_kind = 'blocked'
       AND blocked.blocked_reason = 'execution_failure'
      CROSS JOIN LATERAL (
          SELECT substring(
              sha256(
                  convert_to('signalbox.goal.automatic-resume.v1', 'UTF8')
                  || uuid_send(lineage.session_id)
                  || decode(
                      lpad(to_hex(floor(blocked.event_ordinal / 4294967296)::bigint), 8, '0')
                      || lpad(to_hex(mod(blocked.event_ordinal, 4294967296)::bigint), 8, '0'),
                      'hex'
                  )
              ) FROM 1 FOR 16
          ) AS bytes
      ) AS identity
     WHERE resumed.user_command_id IS NOT NULL
       AND uuid_send(resumed.user_command_id) = set_byte(
           set_byte(
               identity.bytes,
               6,
               (get_byte(identity.bytes, 6) & 15) | 128
           ),
           8,
           (get_byte(identity.bytes, 8) & 63) | 128
       )
), automatic_resumption AS (
    -- $5 and $6 are the deployment's configured automatic-resume attempt budget
    -- and lifetime ceiling, the same two numbers the daemon's resume planner
    -- applies; NULL is the configured unbounded policy for that limit, under
    -- which resumption never ends for that reason. `spent` counts only the
    -- attempts the planner charges and `attempted` counts them all, so a run
    -- ends here at whichever limit ends it there.
    SELECT lineage.session_id,
           ($5::bigint IS NULL OR max(lineage.spent) < $5::bigint)
           AND ($6::bigint IS NULL OR max(lineage.attempted) < $6::bigint)
           AND NOT EXISTS (
               SELECT 1
                 FROM goal_command AS command
                 CROSS JOIN LATERAL (
                     SELECT substring(
                         sha256(
                             convert_to('signalbox.goal.automatic-resume.v1', 'UTF8')
                             || uuid_send(lineage.session_id)
                             || decode(
                                 lpad(to_hex(floor(goal.event_ordinal / 4294967296)::bigint), 8, '0')
                                 || lpad(to_hex(mod(goal.event_ordinal, 4294967296)::bigint), 8, '0'),
                                 'hex'
                             )
                         ) FROM 1 FOR 16
                     ) AS bytes
                 ) AS identity
                WHERE command.result_kind = 'rejected'
                  AND uuid_send(command.command_id) = set_byte(
                      set_byte(
                          identity.bytes,
                          6,
                          (get_byte(identity.bytes, 6) & 15) | 128
                      ),
                      8,
                      (get_byte(identity.bytes, 8) & 63) | 128
                  )
           ) AS pending
      FROM automatic_resume_lineage AS lineage
      JOIN latest_goal AS goal
        ON goal.session_id = lineage.session_id
     GROUP BY lineage.session_id, goal.event_ordinal
), judge AS (
    SELECT facts.*
      FROM operator_attention_judge_facts AS facts JOIN selected USING (session_id)
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
       LEFT(metadata.title, $7) AS title_summary,
       metadata.title IS NOT NULL AND length(metadata.title) > $7 AS title_truncated,
       COALESCE(metadata.archived, false) AS archived,
       facts.active_turn_count::text AS active_turn_count,
       facts.queued_turn_count::text AS queued_turn_count,
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
               -- `unattended_escalation_applies` decides headlessness per
               -- dispatch source, and only repository watch can suppress a
               -- human. A commissioned dispatch takes the unattended path only
               -- when its authority has been withdrawn, and that path
               -- terminalizes the turn rather than parking it, so a
               -- commissioned request still parked in `awaiting_tool_approval`
               -- is always waiting on the commissioning operator.
               AND (
                   NOT (
                       COALESCE(turn.goal_generation = 1, false)
                       AND EXISTS (
                           SELECT 1
                             FROM repo_watch_dispatch_action AS dispatched
                            WHERE dispatched.session_id = selected.session_id
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
               )
           ELSE false
       END AS approval_human_authority,
       goal.generation, goal.event_kind AS goal_state, goal.blocked_reason, goal.need_summary,
       COALESCE(automatic.pending, false) AS goal_automatic_resumption_pending,
       COALESCE(judge.actionable, 0) AS judge_actionable,
       COALESCE(judge.completed, 0) AS judge_completed,
       COALESCE(judge.escalated, 0) AS judge_escalated,
       COALESCE(judge.failed, 0) AS judge_failed,
       runner.state_kind AS runner_state,
       activity.fact_kind, activity.recorded_at
  FROM selected
  LEFT JOIN session_metadata AS metadata
    ON metadata.session_id = selected.session_id
  LEFT JOIN session_timeline_fact AS facts
    ON facts.session_id = selected.session_id
  LEFT JOIN latest_turn AS turn
    ON turn.session_id = selected.session_id
  LEFT JOIN tool_request AS request
    ON request.request_id = turn.approval_tool_request_id
  LEFT JOIN tool_approval_judge_model_call AS approval_call
    ON approval_call.request_id = request.request_id
  LEFT JOIN latest_goal AS goal
    ON goal.session_id = selected.session_id
  LEFT JOIN automatic_resumption AS automatic
    ON automatic.session_id = selected.session_id
  LEFT JOIN judge
    ON judge.session_id = selected.session_id
  LEFT JOIN latest_runner AS runner
    ON runner.session_id = selected.session_id
  LEFT JOIN latest_activity AS activity
    ON activity.session_id = selected.session_id
"#,
            $ordering
        )
    };
}

const SELECT_IDENTITY: &str = summary_sql!(
    r#"
    SELECT session_row.session_id
      FROM session AS session_row
      LEFT JOIN session_metadata AS metadata USING (session_id)
     WHERE ($2::uuid[] IS NOT NULL
            AND session_row.session_id = ANY($2)
            AND NOT COALESCE(metadata.archived, false))
        OR ($2::uuid[] IS NULL
            AND ($3::uuid IS NULL OR session_row.session_id > $3)
            AND ($8::text IS NULL
                 OR strpos(COALESCE(metadata.title, ''), $8) > 0
                 OR strpos(session_row.session_id::text, $8) > 0)
            AND ($10 OR NOT COALESCE(metadata.archived, false))
            AND NOT EXISTS (
                SELECT 1 FROM unnest($9::text[]) AS required(tag)
                 WHERE NOT EXISTS (
                    SELECT 1 FROM session_metadata_tag AS stored
                     WHERE stored.session_id = session_row.session_id
                       AND stored.tag = required.tag))
            AND $11::timestamptz IS NULL)
     ORDER BY session_row.session_id LIMIT $1
    "#,
    "ORDER BY selected.session_id"
);

// The indexed timestamp projection chooses one bounded keyset page. The
// sequence-backed journal remains authoritative for the activity kind and
// timestamp decoded into each selected summary.
const SELECT_LAST_ACTIVITY: &str = summary_sql!(
    r#"
    SELECT session_row.session_id
      FROM session_timeline_fact AS facts
      JOIN session AS session_row USING (session_id)
      LEFT JOIN session_metadata AS metadata USING (session_id)
     WHERE ($8::text IS NULL
            OR strpos(COALESCE(metadata.title, ''), $8) > 0
            OR strpos(session_row.session_id::text, $8) > 0)
       AND ($10 OR NOT COALESCE(metadata.archived, false))
       AND NOT EXISTS (
           SELECT 1 FROM unnest($9::text[]) AS required(tag)
            WHERE NOT EXISTS (
               SELECT 1 FROM session_metadata_tag AS stored
                WHERE stored.session_id = session_row.session_id
                  AND stored.tag = required.tag))
       AND ($11::timestamptz IS NULL
            OR facts.attention_activity_recorded_at < $11
            OR (facts.attention_activity_recorded_at = $11
                AND session_row.session_id > $3))
     ORDER BY facts.attention_activity_recorded_at DESC, session_row.session_id LIMIT $1
    "#,
    "ORDER BY activity.recorded_at DESC, selected.session_id"
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

pub(crate) async fn load_summaries(
    transaction: &mut Transaction<'_, Postgres>,
    identities: Option<&[Uuid]>,
    query: Option<&AttentionQuery>,
    automatic_resume_attempts: AutomaticResumeAttemptBounds,
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
        .bind(automatic_resume_attempts.budget.map(i64::from))
        .bind(automatic_resume_attempts.ceiling.map(i64::from))
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

/// Fails the coherent catalog read closed when any durable session is missing
/// its indexed timeline/activity fact or its projected key disagrees with the
/// authoritative latest journal row. The exact count is session-driven, so an
/// incomplete projection must not silently shrink or misorder the page.
async fn verify_fact_completeness(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), AttentionRepositoryError> {
    let incomplete = sqlx::query_scalar::<_, bool>(
        "SELECT facts.session_id IS NULL
                OR facts.attention_activity_recorded_at IS NULL
                OR activity.recorded_at IS NULL AS missing
           FROM session AS session_row
           LEFT JOIN session_timeline_fact AS facts USING (session_id)
           LEFT JOIN LATERAL (
               SELECT change.recorded_at
                 FROM operator_attention_change AS change
                WHERE change.session_id = session_row.session_id
                ORDER BY change.change_sequence DESC
                LIMIT 1
           ) AS activity ON true
          WHERE facts.session_id IS NULL
             OR facts.attention_activity_recorded_at IS NULL
             OR activity.recorded_at IS NULL
             OR facts.attention_activity_recorded_at IS DISTINCT FROM activity.recorded_at
          LIMIT 1",
    )
    .fetch_optional(&mut **transaction)
    .await?;
    match incomplete {
        Some(true) => Err(AttentionCorruption::Missing("session activity fact").into()),
        Some(false) => Err(AttentionCorruption::Invalid("session activity fact").into()),
        None => Ok(()),
    }
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
    let approval_human_authority = row
        .try_get::<Option<bool>, _>("approval_human_authority")?
        .unwrap_or(false);
    let automatic_resumption_pending = row
        .try_get::<Option<bool>, _>("goal_automatic_resumption_pending")?
        .unwrap_or(false);
    let action = attention_action(
        state,
        approval_human_authority,
        automatic_resumption_pending,
    );
    let active_turn_count = required_string(row, "active_turn_count")?
        .parse()
        .map_err(|_| AttentionCorruption::Invalid("active turn count"))?;
    let queued_turn_count = required_string(row, "queued_turn_count")?
        .parse()
        .map_err(|_| AttentionCorruption::Invalid("queued turn count"))?;
    validate_state_counts(state, active_turn_count, queued_turn_count)?;
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
        active_turn_count,
        queued_turn_count,
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

fn validate_state_counts(
    state: AttentionState,
    active_turn_count: u64,
    queued_turn_count: u64,
) -> Result<(), AttentionRepositoryError> {
    let active_backed = matches!(
        state,
        AttentionState::Active
            | AttentionState::AwaitingApproval
            | AttentionState::Ambiguous
            | AttentionState::AwaitingToolRecovery
    );
    if active_backed && active_turn_count == 0 {
        return Err(AttentionCorruption::Invalid("active turn count").into());
    }
    if state == AttentionState::Queued && queued_turn_count == 0 {
        return Err(AttentionCorruption::Invalid("queued turn count").into());
    }
    Ok(())
}

fn attention_action(
    state: AttentionState,
    approval_human_authority: bool,
    automatic_resumption_pending: bool,
) -> Option<AttentionAction> {
    match state {
        AttentionState::Blocked if !automatic_resumption_pending => {
            Some(AttentionAction::ProvideGoalNeed)
        }
        AttentionState::Blocked => None,
        AttentionState::AwaitingApproval if approval_human_authority => {
            Some(AttentionAction::DecideApproval)
        }
        AttentionState::Ambiguous => Some(AttentionAction::ReconcileTurn),
        AttentionState::AwaitingApproval
        | AttentionState::AwaitingToolRecovery
        | AttentionState::AwaitingReconciliation
        | AttentionState::RunnerLost
        | AttentionState::Active
        | AttentionState::Queued
        | AttentionState::Idle => None,
    }
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

    #[test]
    fn active_state_requires_a_projected_active_turn() {
        let error = validate_state_counts(AttentionState::Active, 0, 0)
            .expect_err("an active state with no projected active turn is corrupt");

        assert!(matches!(
            error,
            AttentionRepositoryError::Corruption(AttentionCorruption::Invalid("active turn count"))
        ));
    }

    #[test]
    fn queued_state_requires_a_projected_queued_turn() {
        let error = validate_state_counts(AttentionState::Queued, 0, 0)
            .expect_err("a queued state with no projected queued turn is corrupt");

        assert!(matches!(
            error,
            AttentionRepositoryError::Corruption(AttentionCorruption::Invalid("queued turn count"))
        ));
    }

    #[test]
    fn automatic_goal_resumption_suppresses_operator_action() {
        assert_eq!(attention_action(AttentionState::Blocked, false, true), None);
        assert_eq!(
            attention_action(AttentionState::Blocked, false, false),
            Some(AttentionAction::ProvideGoalNeed)
        );
    }
}
