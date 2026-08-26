//! Coherent bounded operator projections over durable repository-watch facts.

use std::{collections::BTreeMap, error::Error, fmt, num::NonZeroU64, time::SystemTime};

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_application::{
    RepoWatchActivityPage, RepoWatchAutomationStatus, RepoWatchEventCursor,
    RepoWatchEventKindCount, RepoWatchHeldCursor, RepoWatchHeldSlot, RepoWatchHeldSlotBlocker,
    RepoWatchLatestWebhook, RepoWatchObligationCursor, RepoWatchObligationId,
    RepoWatchObligationReadiness, RepoWatchOperationsReader, RepoWatchOperatorDispatch,
    RepoWatchOperatorEvent, RepoWatchOperatorSettlement, RepoWatchPagePosition,
    RepoWatchPullRequestOperations, RepoWatchPullRequestOperationsFacts, RepoWatchPullRequestPage,
    RepoWatchPullRequestSession, RepoWatchPullRequestSessionPage, RepoWatchQueuedObligation,
    RepoWatchRepositoryStatus, RepoWatchRepositoryStatusPage, RepoWatchSessionCursor,
    RepoWatchSessionPurpose, RepoWatchSingletonKey, RepoWatchWebhookActivity,
    RepoWatchWebhookDisposition, RepoWatchWebhookWindow, RepoWatchWorkPage,
    max_repo_watch_activity_page_items, max_repo_watch_operations_page_items,
};
use signalbox_domain::{
    CommissionedDispatchId, PullRequestNumber, RepoWatchDispatchId, RepoWatchEventId,
    RepoWatchEventKindNameV1, RepoWatchRuleId, RepositorySlug, SessionId,
};
use sqlx::{
    PgPool, Postgres, Row, Transaction,
    postgres::PgRow,
    types::{Uuid, time::OffsetDateTime},
};

use crate::{
    attention::{AttentionRepositoryError, load_summaries},
    mapping::{
        positive_u64_from_numeric, repo_watch_event_kind_from_str,
        repo_watch_webhook_disposition_from_str,
    },
    repo_watch::{RepoWatchStoreError, decode_current_pull_request},
    repo_watch_webhook::RepoWatchWebhookDisposition as StoredWebhookDisposition,
};

const FIVE_MINUTES_SECONDS: u32 = 5 * 60;
const ONE_HOUR_SECONDS: u32 = 60 * 60;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchOperationsCorruption {
    Invalid(&'static str),
    Unsupported { field: &'static str, value: String },
}

impl fmt::Display for RepoWatchOperationsCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(field) => {
                write!(formatter, "invalid repository-watch operations {field}")
            }
            Self::Unsupported { field, value } => {
                write!(
                    formatter,
                    "unsupported repository-watch operations {field}: {value}"
                )
            }
        }
    }
}

impl Error for RepoWatchOperationsCorruption {}

#[derive(Debug)]
pub enum RepoWatchOperationsError {
    Database(sqlx::Error),
    Attention(AttentionRepositoryError),
    RepoWatch(RepoWatchStoreError),
    Corruption(RepoWatchOperationsCorruption),
}

impl fmt::Display for RepoWatchOperationsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(
                formatter,
                "repository-watch operations database failure: {error}"
            ),
            Self::Attention(error) => error.fmt(formatter),
            Self::RepoWatch(error) => error.fmt(formatter),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for RepoWatchOperationsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Attention(error) => Some(error),
            Self::RepoWatch(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for RepoWatchOperationsError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<AttentionRepositoryError> for RepoWatchOperationsError {
    fn from(error: AttentionRepositoryError) -> Self {
        Self::Attention(error)
    }
}

impl From<RepoWatchStoreError> for RepoWatchOperationsError {
    fn from(error: RepoWatchStoreError) -> Self {
        Self::RepoWatch(error)
    }
}

impl From<RepoWatchOperationsCorruption> for RepoWatchOperationsError {
    fn from(error: RepoWatchOperationsCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// Read-only PostgreSQL implementation of repository-watch operator queries.
#[derive(Clone, Debug)]
pub struct PostgresRepoWatchOperations {
    pool: PgPool,
    automatic_resume_attempt_budget: Option<u32>,
}

impl PostgresRepoWatchOperations {
    /// Binds the operator projection to the deployment's automatic-resume
    /// attempt budget.
    ///
    /// The pull-request session reads carry the same attention summaries the
    /// fleet projection serves, so this must be the budget the daemon's resume
    /// planner applies (`automatic_resume_attempt_budget`); reading a
    /// different number makes the projection report a session as needing its
    /// operator while the daemon still owes it resumes, or the reverse. `None`
    /// is the configured unbounded budget, under which automatic resumption
    /// never exhausts.
    #[must_use]
    pub const fn new(pool: PgPool, automatic_resume_attempt_budget: Option<u32>) -> Self {
        Self {
            pool,
            automatic_resume_attempt_budget,
        }
    }

    async fn read_transaction(
        &self,
    ) -> Result<Transaction<'_, Postgres>, RepoWatchOperationsError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await?;
        Ok(transaction)
    }

    pub async fn repository_statuses(
        &self,
        after: Option<RepositorySlug>,
    ) -> Result<RepoWatchRepositoryStatusPage, RepoWatchOperationsError> {
        let mut transaction = self.read_transaction().await?;
        let rows = sqlx::query(REPOSITORY_STATUS_SQL)
            .bind(after.as_ref().map(RepositorySlug::as_str))
            .bind(i64::from(max_repo_watch_operations_page_items()) + 1)
            .fetch_all(&mut *transaction)
            .await?;
        let has_more = rows.len() > usize::from(max_repo_watch_operations_page_items());
        let retained = &rows[..rows
            .len()
            .min(usize::from(max_repo_watch_operations_page_items()))];
        let repositories = retained
            .iter()
            .map(|row| row.try_get::<String, _>("repository"))
            .collect::<Result<Vec<_>, _>>()?;
        let mut event_counts = load_event_kind_counts(&mut transaction, &repositories).await?;
        let statuses = retained
            .iter()
            .map(|row| {
                decode_repository_status(
                    row,
                    event_counts
                        .remove(&row.try_get::<String, _>("repository")?)
                        .unwrap_or_default(),
                )
            })
            .collect::<Result<Vec<_>, RepoWatchOperationsError>>()?;
        let continuation_after = has_more
            .then(|| statuses.last().map(|status| status.repository.clone()))
            .flatten();
        transaction.commit().await?;
        Ok(RepoWatchRepositoryStatusPage {
            repositories: statuses,
            continuation_after,
        })
    }

    pub async fn pull_requests(
        &self,
        repository: RepositorySlug,
        after: Option<PullRequestNumber>,
    ) -> Result<RepoWatchPullRequestPage, RepoWatchOperationsError> {
        let mut transaction = self.read_transaction().await?;
        let rows = sqlx::query(CURRENT_PULL_REQUEST_PAGE_SQL)
            .bind(repository.as_str())
            .bind(after.map(|number| Decimal::from(number.get())))
            .bind(i64::from(max_repo_watch_operations_page_items()) + 1)
            .fetch_all(&mut *transaction)
            .await?;
        let has_more = rows.len() > usize::from(max_repo_watch_operations_page_items());
        let selected = rows
            .iter()
            .take(usize::from(max_repo_watch_operations_page_items()))
            .map(|row| {
                let state = decode_current_pull_request(
                    row.try_get::<sqlx::types::Json<Value>, _>("state_payload")?
                        .0,
                )?;
                let open_parent = optional_pull_request(row.try_get("open_parent")?)?;
                let open_child_count =
                    nonnegative(row.try_get("open_child_count")?, "open child count")?;
                Ok::<_, RepoWatchOperationsError>((state, open_parent, open_child_count))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let numbers = selected
            .iter()
            .map(|(pull_request, _, _)| Decimal::from(pull_request.context().number().get()))
            .collect::<Vec<_>>();
        let rows = load_pull_request_fact_rows(&mut transaction, &repository, &numbers).await?;
        let mut facts = rows
            .iter()
            .map(decode_pull_request_fact_row)
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let pull_requests = selected
            .iter()
            .map(|(state, open_parent, open_child_count)| {
                let number = state.context().number();
                let stored = facts
                    .remove(&number.get())
                    .unwrap_or_else(StoredPullRequestFacts::empty);
                RepoWatchPullRequestOperations::from_state(
                    state,
                    stored.into_application(
                        state.context().head_sha().as_str(),
                        *open_parent,
                        *open_child_count,
                    ),
                )
            })
            .collect::<Vec<_>>();
        let continuation_after = has_more
            .then(|| pull_requests.last().map(|pull_request| pull_request.number))
            .flatten();
        transaction.commit().await?;
        Ok(RepoWatchPullRequestPage {
            repository,
            pull_requests,
            continuation_after,
        })
    }

    pub async fn work(
        &self,
        repository: RepositorySlug,
        held_after: RepoWatchPagePosition<RepoWatchHeldCursor>,
        obligation_after: RepoWatchPagePosition<RepoWatchObligationCursor>,
    ) -> Result<RepoWatchWorkPage, RepoWatchOperationsError> {
        let mut transaction = self.read_transaction().await?;
        let held_rows = match held_after {
            RepoWatchPagePosition::Exhausted => Vec::new(),
            RepoWatchPagePosition::Start | RepoWatchPagePosition::After(_) => {
                let cursor = match held_after {
                    RepoWatchPagePosition::After(cursor) => Some(cursor),
                    RepoWatchPagePosition::Start | RepoWatchPagePosition::Exhausted => None,
                };
                sqlx::query(HELD_SLOTS_SQL)
                    .bind(repository.as_str())
                    .bind(cursor.map(|cursor| OffsetDateTime::from(cursor.held_since)))
                    .bind(cursor.map(|cursor| cursor.dispatch.into_uuid()))
                    .bind(i64::from(max_repo_watch_operations_page_items()) + 1)
                    .fetch_all(&mut *transaction)
                    .await?
            }
        };
        let obligation_rows = match obligation_after {
            RepoWatchPagePosition::Exhausted => Vec::new(),
            RepoWatchPagePosition::Start | RepoWatchPagePosition::After(_) => {
                let cursor = match obligation_after {
                    RepoWatchPagePosition::After(cursor) => Some(cursor),
                    RepoWatchPagePosition::Start | RepoWatchPagePosition::Exhausted => None,
                };
                sqlx::query(OBLIGATIONS_SQL)
                    .bind(repository.as_str())
                    .bind(cursor.map(|cursor| OffsetDateTime::from(cursor.owed_since)))
                    .bind(cursor.map(|cursor| cursor.obligation.into_uuid()))
                    .bind(i64::from(max_repo_watch_operations_page_items()) + 1)
                    .fetch_all(&mut *transaction)
                    .await?
            }
        };
        let held_has_more = held_rows.len() > usize::from(max_repo_watch_operations_page_items());
        let obligation_has_more =
            obligation_rows.len() > usize::from(max_repo_watch_operations_page_items());
        let held_slots = held_rows
            .iter()
            .take(usize::from(max_repo_watch_operations_page_items()))
            .map(decode_held_slot)
            .collect::<Result<Vec<_>, _>>()?;
        let queued_obligations = obligation_rows
            .iter()
            .take(usize::from(max_repo_watch_operations_page_items()))
            .map(decode_obligation)
            .collect::<Result<Vec<_>, _>>()?;
        let held_continuation_after = if held_has_more {
            let Some(slot) = held_slots.last() else {
                return Err(
                    RepoWatchOperationsCorruption::Invalid("held continuation page").into(),
                );
            };
            RepoWatchPagePosition::After(RepoWatchHeldCursor {
                held_since: slot.held_since,
                dispatch: slot.dispatch,
            })
        } else {
            RepoWatchPagePosition::Exhausted
        };
        let obligation_continuation_after = if obligation_has_more {
            let Some(obligation) = queued_obligations.last() else {
                return Err(
                    RepoWatchOperationsCorruption::Invalid("obligation continuation page").into(),
                );
            };
            RepoWatchPagePosition::After(RepoWatchObligationCursor {
                owed_since: obligation.owed_since,
                obligation: obligation.id,
            })
        } else {
            RepoWatchPagePosition::Exhausted
        };
        transaction.commit().await?;
        Ok(RepoWatchWorkPage {
            held_slots,
            held_continuation_after,
            queued_obligations,
            obligation_continuation_after,
        })
    }

    pub async fn pull_request_sessions(
        &self,
        repository: RepositorySlug,
        pull_request: PullRequestNumber,
        before: Option<RepoWatchSessionCursor>,
    ) -> Result<RepoWatchPullRequestSessionPage, RepoWatchOperationsError> {
        let mut transaction = self.read_transaction().await?;
        let rows = sqlx::query(PULL_REQUEST_SESSIONS_SQL)
            .bind(repository.as_str())
            .bind(Decimal::from(pull_request.get()))
            .bind(before.map(|cursor| OffsetDateTime::from(cursor.commissioned_at)))
            .bind(before.map(|cursor| cursor.session.into_uuid()))
            .bind(i64::from(max_repo_watch_operations_page_items()) + 1)
            .fetch_all(&mut *transaction)
            .await?;
        let has_more = rows.len() > usize::from(max_repo_watch_operations_page_items());
        let retained = rows
            .iter()
            .take(usize::from(max_repo_watch_operations_page_items()))
            .collect::<Vec<_>>();
        let identities = retained
            .iter()
            .map(|row| row.try_get::<Uuid, _>("session_id"))
            .collect::<Result<Vec<_>, _>>()?;
        let summaries = load_summaries(
            &mut transaction,
            Some(&identities),
            None,
            self.automatic_resume_attempt_budget,
        )
        .await?;
        let mut summaries = summaries
            .into_iter()
            .map(|summary| (summary.session, summary))
            .collect::<BTreeMap<_, _>>();
        let sessions = retained
            .iter()
            .map(|row| decode_pull_request_session(row, &mut summaries))
            .collect::<Result<Vec<_>, _>>()?;
        let continuation_before = has_more
            .then(|| {
                sessions.last().map(|session| RepoWatchSessionCursor {
                    commissioned_at: session.commissioned_at,
                    session: session.attention.session,
                })
            })
            .flatten();
        transaction.commit().await?;
        Ok(RepoWatchPullRequestSessionPage {
            sessions,
            continuation_before,
        })
    }

    pub async fn activity(
        &self,
        repository: RepositorySlug,
        events_before: RepoWatchPagePosition<RepoWatchEventCursor>,
        webhooks_before: RepoWatchPagePosition<u64>,
    ) -> Result<RepoWatchActivityPage, RepoWatchOperationsError> {
        let mut transaction = self.read_transaction().await?;
        let event_rows = match events_before {
            RepoWatchPagePosition::Exhausted => Vec::new(),
            RepoWatchPagePosition::Start | RepoWatchPagePosition::After(_) => {
                let cursor = match events_before {
                    RepoWatchPagePosition::After(cursor) => Some(cursor),
                    RepoWatchPagePosition::Start | RepoWatchPagePosition::Exhausted => None,
                };
                sqlx::query(ACTIVITY_EVENTS_SQL)
                    .bind(repository.as_str())
                    .bind(
                        cursor
                            .map(|cursor| i64::try_from(cursor.cursor_generation))
                            .transpose()
                            .map_err(|_| {
                                RepoWatchOperationsCorruption::Invalid("event cursor generation")
                            })?,
                    )
                    .bind(
                        cursor
                            .map(|cursor| i32::try_from(cursor.event_ordinal))
                            .transpose()
                            .map_err(|_| {
                                RepoWatchOperationsCorruption::Invalid("event cursor ordinal")
                            })?,
                    )
                    .bind(i64::from(max_repo_watch_activity_page_items()) + 1)
                    .fetch_all(&mut *transaction)
                    .await?
            }
        };
        let webhook_rows =
            match webhooks_before {
                RepoWatchPagePosition::Exhausted => Vec::new(),
                RepoWatchPagePosition::Start | RepoWatchPagePosition::After(_) => {
                    let cursor = match webhooks_before {
                        RepoWatchPagePosition::After(cursor) => Some(cursor),
                        RepoWatchPagePosition::Start | RepoWatchPagePosition::Exhausted => None,
                    };
                    sqlx::query(ACTIVITY_WEBHOOKS_SQL)
                        .bind(repository.as_str())
                        .bind(cursor.map(i64::try_from).transpose().map_err(|_| {
                            RepoWatchOperationsCorruption::Invalid("webhook cursor")
                        })?)
                        .bind(i64::from(max_repo_watch_activity_page_items()) + 1)
                        .fetch_all(&mut *transaction)
                        .await?
                }
            };
        let event_has_more = event_rows.len() > usize::from(max_repo_watch_activity_page_items());
        let webhook_has_more =
            webhook_rows.len() > usize::from(max_repo_watch_activity_page_items());
        let events = event_rows
            .iter()
            .take(usize::from(max_repo_watch_activity_page_items()))
            .map(decode_activity_event)
            .collect::<Result<Vec<_>, _>>()?;
        let webhooks = webhook_rows
            .iter()
            .take(usize::from(max_repo_watch_activity_page_items()))
            .map(decode_webhook_activity)
            .collect::<Result<Vec<_>, _>>()?;
        let event_continuation_before = if event_has_more {
            let Some(event) = events.last() else {
                return Err(
                    RepoWatchOperationsCorruption::Invalid("event continuation page").into(),
                );
            };
            RepoWatchPagePosition::After(RepoWatchEventCursor {
                cursor_generation: event.cursor_generation,
                event_ordinal: event.event_ordinal,
            })
        } else {
            RepoWatchPagePosition::Exhausted
        };
        let webhook_continuation_before = if webhook_has_more {
            let Some(webhook) = webhooks.last() else {
                return Err(
                    RepoWatchOperationsCorruption::Invalid("webhook continuation page").into(),
                );
            };
            RepoWatchPagePosition::After(webhook.receipt_sequence)
        } else {
            RepoWatchPagePosition::Exhausted
        };
        transaction.commit().await?;
        Ok(RepoWatchActivityPage {
            events,
            event_continuation_before,
            webhooks,
            webhook_continuation_before,
        })
    }
}

impl RepoWatchOperationsReader for PostgresRepoWatchOperations {
    type Error = RepoWatchOperationsError;

    async fn repository_statuses(
        &self,
        after: Option<RepositorySlug>,
    ) -> Result<RepoWatchRepositoryStatusPage, Self::Error> {
        Self::repository_statuses(self, after).await
    }

    async fn pull_requests(
        &self,
        repository: RepositorySlug,
        after: Option<PullRequestNumber>,
    ) -> Result<RepoWatchPullRequestPage, Self::Error> {
        Self::pull_requests(self, repository, after).await
    }

    async fn work(
        &self,
        repository: RepositorySlug,
        held_after: RepoWatchPagePosition<RepoWatchHeldCursor>,
        obligation_after: RepoWatchPagePosition<RepoWatchObligationCursor>,
    ) -> Result<RepoWatchWorkPage, Self::Error> {
        Self::work(self, repository, held_after, obligation_after).await
    }

    async fn pull_request_sessions(
        &self,
        repository: RepositorySlug,
        pull_request: PullRequestNumber,
        before: Option<RepoWatchSessionCursor>,
    ) -> Result<RepoWatchPullRequestSessionPage, Self::Error> {
        Self::pull_request_sessions(self, repository, pull_request, before).await
    }

    async fn activity(
        &self,
        repository: RepositorySlug,
        events_before: RepoWatchPagePosition<RepoWatchEventCursor>,
        webhooks_before: RepoWatchPagePosition<u64>,
    ) -> Result<RepoWatchActivityPage, Self::Error> {
        Self::activity(self, repository, events_before, webhooks_before).await
    }
}

const HELD_SLOTS_SQL: &str = r#"
SELECT slot.dispatch_id, slot.singleton_scope, slot.singleton_repository,
       slot.singleton_pull_request_number,
       slot.singleton_stack_root_pull_request_number,
       slot.rule_id, current.held_since, slot.session_ids, slot.blockers
  FROM repo_watch_current_held_dispatch AS current
  JOIN repo_watch_held_dispatch_slot AS slot USING (dispatch_id)
 WHERE current.repository = $1
   AND ($2::timestamptz IS NULL OR (current.held_since, current.dispatch_id) > ($2, $3))
 ORDER BY current.held_since, current.dispatch_id
 LIMIT $4
"#;

const OBLIGATIONS_SQL: &str = r#"
SELECT obligation_id, singleton_scope, singleton_repository,
       singleton_pull_request_number, singleton_stack_root_pull_request_number,
       rule_id, first_repository, first_event_id,
       latest_event_id, matched_event_count, owed_since, latest_match_at,
       failed_attempts, occupying_dispatch_id, occupying_session_ids,
       CASE WHEN effective_eligible_at = 'infinity'::timestamptz
            THEN NULL ELSE effective_eligible_at END
           AS eligible_at,
       COALESCE(effective_eligible_at = 'infinity'::timestamptz, false)
           AS eligibility_is_infinite,
       -- The view already conjoins every blocker the dispatch loader honours,
       -- including the live external session that owns no dispatch identity.
       -- Conjoin it rather than restating it, and add only the failure backoff
       -- the view does not know about, so this read cannot call an obligation
       -- ready that admission refuses.
       obligation.ready
           AND (effective_eligible_at IS NULL
                OR effective_eligible_at <= clock_timestamp()) AS ready,
       occupying_dispatch_id IS NULL
           AND occupying_session_ids IS NOT NULL AS externally_blocked,
       parked_at
  FROM repo_watch_outstanding_dispatch_obligation AS obligation
  CROSS JOIN LATERAL (
        SELECT GREATEST(
            obligation.eligible_at,
            CASE WHEN obligation.last_failed_attempt_at IS NULL THEN NULL
                 ELSE obligation.last_failed_attempt_at + LEAST(
                     600::bigint << LEAST(
                         GREATEST(obligation.failed_attempts - 1, 0), 30
                     )::integer,
                     3600::bigint
                 ) * interval '1 second'
            END
        ) AS effective_eligible_at
  ) AS eligibility
 WHERE repository = $1
   AND ($2::timestamptz IS NULL OR (owed_since, obligation_id) > ($2, $3))
 ORDER BY owed_since, obligation_id
 LIMIT $4
"#;

const PULL_REQUEST_SESSIONS_SQL: &str = r#"
SELECT * FROM (
    SELECT action.session_id, action.recorded_at AS commissioned_at,
           'rule_dispatch'::text AS purpose_kind, action.dispatch_id,
           action.event_id, batch.rule_id, action.template_name
      FROM repo_watch_dispatch_action AS action
      JOIN repo_watch_dispatch_batch AS batch ON batch.dispatch_id = action.dispatch_id
     WHERE action.repository = $1 AND action.pull_request_number = $2
    UNION ALL
    SELECT commissioned.session_id, commissioned.recorded_at AS commissioned_at,
           'operator_commission'::text AS purpose_kind, commissioned.dispatch_id,
           NULL::uuid AS event_id, NULL::text AS rule_id, commissioned.template_name
      FROM commissioned_dispatch AS commissioned
     WHERE commissioned.repository = $1
       AND commissioned.pull_request_number = $2
       AND commissioned.target_kind = 'pull_request'
) AS correlated
 WHERE ($3::timestamptz IS NULL OR (commissioned_at, session_id) < ($3, $4))
 ORDER BY commissioned_at DESC, session_id DESC
 LIMIT $5
"#;

const ACTIVITY_EVENTS_SQL: &str = r#"
SELECT event_id, cursor_generation, event_ordinal, event_kind,
       pull_request_number, recorded_at
  FROM repo_watch_event
 WHERE repository = $1
   AND ($2::bigint IS NULL OR (cursor_generation, event_ordinal) < ($2, $3))
 ORDER BY cursor_generation DESC, event_ordinal DESC
 LIMIT $4
"#;

const ACTIVITY_WEBHOOKS_SQL: &str = r#"
SELECT delivery.receipt_sequence, delivery.event_name, delivery.action_name,
       delivery.received_at, count(projection.projection_ordinal) AS projection_count,
       max(projection.projected_at) AS latest_projected_at,
       disposition.disposition
  FROM repo_watch_webhook_delivery AS delivery
  LEFT JOIN repo_watch_webhook_projection AS projection
    ON projection.hook_id = delivery.hook_id
   AND projection.delivery_id = delivery.delivery_id
  LEFT JOIN repo_watch_webhook_disposition AS disposition
    ON disposition.hook_id = delivery.hook_id
   AND disposition.delivery_id = delivery.delivery_id
 WHERE delivery.repository = $1
   AND ($2::bigint IS NULL OR delivery.receipt_sequence < $2)
 GROUP BY delivery.hook_id, delivery.delivery_id, delivery.receipt_sequence,
          delivery.event_name, delivery.action_name, delivery.received_at,
          disposition.disposition
 ORDER BY delivery.receipt_sequence DESC
 LIMIT $3
"#;

const CURRENT_PULL_REQUEST_PAGE_SQL: &str = r#"
WITH selected_subjects AS (
    SELECT *
      FROM repo_watch_current_pull_request
     WHERE repository = $1
       AND ($2::numeric IS NULL OR pull_request_number > $2)
     ORDER BY pull_request_number
     LIMIT $3
), child_counts AS (
    SELECT subject.pull_request_number,
           count(candidate.pull_request_number) AS open_child_count
      FROM selected_subjects AS subject
      LEFT JOIN repo_watch_current_pull_request AS candidate
        ON candidate.repository = subject.repository
       AND candidate.lifecycle = 'open'
       AND candidate.pull_request_number <> subject.pull_request_number
       AND candidate.base_branch = subject.head_branch
     WHERE subject.lifecycle = 'open'
       AND subject.head_repository = $1
     GROUP BY subject.pull_request_number
)
SELECT subject.state_payload,
       CASE WHEN subject.lifecycle = 'open' THEN parent.pull_request_number END
           AS open_parent,
       CASE
           WHEN subject.lifecycle = 'open' AND subject.head_repository = $1
               THEN COALESCE(child_counts.open_child_count, 0)
           ELSE 0
       END AS open_child_count
  FROM selected_subjects AS subject
  LEFT JOIN LATERAL (
        SELECT candidate.pull_request_number
          FROM repo_watch_current_pull_request AS candidate
         WHERE candidate.repository = subject.repository
           AND candidate.lifecycle = 'open'
           AND candidate.pull_request_number <> subject.pull_request_number
           AND candidate.head_repository = subject.repository
           AND candidate.head_branch = subject.base_branch
         ORDER BY candidate.pull_request_number
         LIMIT 1
  ) AS parent ON true
  LEFT JOIN child_counts
    ON child_counts.pull_request_number = subject.pull_request_number
 ORDER BY subject.pull_request_number
"#;

const REPOSITORY_STATUS_SQL: &str = r#"
WITH selected_repositories AS (
    SELECT repository
      FROM repo_watch_repository_key
     WHERE ($1::text IS NULL OR repository COLLATE "C" > $1 COLLATE "C")
     ORDER BY repository COLLATE "C"
     LIMIT $2
), selected AS (
    SELECT selected_repository.repository, latest.generation, latest.recorded_at
      FROM selected_repositories AS selected_repository
      LEFT JOIN LATERAL (
            SELECT generation, recorded_at
              FROM repo_watch_cursor
             WHERE repository = selected_repository.repository
             ORDER BY generation DESC
             LIMIT 1
      ) AS latest ON true
)
SELECT selected.repository, selected.generation, selected.recorded_at,
       webhook.receipt_sequence, webhook.event_name, webhook.action_name,
       webhook.received_at,
       COALESCE(webhook_window.received_5m, 0) AS received_5m,
       COALESCE(webhook_window.projected_5m, 0) AS projected_5m,
       COALESCE(webhook_window.terminal_5m, 0) AS terminal_5m,
       COALESCE(webhook_window.quarantined_5m, 0) AS quarantined_5m,
       COALESCE(webhook_window.received_1h, 0) AS received_1h,
       COALESCE(webhook_window.projected_1h, 0) AS projected_1h,
       COALESCE(webhook_window.terminal_1h, 0) AS terminal_1h,
       COALESCE(webhook_window.quarantined_1h, 0) AS quarantined_1h,
       latest_latency.latest_latency_ms, latency_window.maximum_latency_ms_1h,
       observed.event_id AS observed_event_id,
       observed.cursor_generation AS observed_generation,
       observed.event_ordinal AS observed_ordinal,
       observed.event_kind AS observed_kind,
       observed.pull_request_number AS observed_pull_request,
       observed.recorded_at AS observed_at,
       actionable.event_id AS actionable_event_id,
       actionable.cursor_generation AS actionable_generation,
       actionable.event_ordinal AS actionable_ordinal,
       actionable.event_kind AS actionable_kind,
       actionable.pull_request_number AS actionable_pull_request,
       actionable.recorded_at AS actionable_at,
       dispatch.dispatch_id, dispatch.event_id AS dispatch_event_id,
       dispatch.rule_id AS dispatch_rule_id, dispatch.admitted_at AS dispatch_at,
       settlement.dispatch_id AS settlement_dispatch_id,
       settlement.event_id AS settlement_event_id,
       settlement.released_at AS settlement_at,
       COALESCE(held.held_count, 0) AS held_count,
       COALESCE(queued.obligation_count, 0) AS queued_count
  FROM selected
  LEFT JOIN LATERAL (
        SELECT receipt_sequence, event_name, action_name, received_at
          FROM repo_watch_webhook_delivery
         WHERE repository = selected.repository
         ORDER BY receipt_sequence DESC LIMIT 1
  ) AS webhook ON true
  LEFT JOIN LATERAL (
        SELECT count(*) FILTER (WHERE delivery.received_at >= transaction_timestamp() - interval '5 minutes') AS received_5m,
               count(*) FILTER (WHERE delivery.received_at >= transaction_timestamp() - interval '5 minutes'
                                  AND projection.hook_id IS NOT NULL) AS projected_5m,
               count(*) FILTER (WHERE delivery.received_at >= transaction_timestamp() - interval '5 minutes'
                                  AND disposition.hook_id IS NOT NULL) AS terminal_5m,
               count(*) FILTER (WHERE delivery.received_at >= transaction_timestamp() - interval '5 minutes'
                                  AND disposition.disposition = 'quarantined') AS quarantined_5m,
               count(*) AS received_1h,
               count(*) FILTER (WHERE projection.hook_id IS NOT NULL) AS projected_1h,
               count(*) FILTER (WHERE disposition.hook_id IS NOT NULL) AS terminal_1h,
               count(*) FILTER (WHERE disposition.disposition = 'quarantined') AS quarantined_1h
          FROM repo_watch_webhook_delivery AS delivery
          LEFT JOIN LATERAL (
                SELECT candidate.hook_id
                  FROM repo_watch_webhook_projection AS candidate
                 WHERE candidate.hook_id = delivery.hook_id
                   AND candidate.delivery_id = delivery.delivery_id
                 LIMIT 1
          ) AS projection ON true
          LEFT JOIN repo_watch_webhook_disposition AS disposition
            ON disposition.hook_id = delivery.hook_id
           AND disposition.delivery_id = delivery.delivery_id
         WHERE delivery.repository = selected.repository
           AND delivery.received_at >= transaction_timestamp() - interval '1 hour'
  ) AS webhook_window ON true
  LEFT JOIN LATERAL (
        SELECT floor(extract(epoch FROM (projection.projected_at - projection.received_at)) * 1000)::bigint
                   AS latest_latency_ms
          FROM repo_watch_webhook_projection AS projection
         WHERE projection.repository = selected.repository
         ORDER BY projection.projected_at DESC, projection.delivery_id DESC,
                  projection.projection_ordinal DESC
         LIMIT 1
  ) AS latest_latency ON true
  LEFT JOIN LATERAL (
        SELECT max(floor(extract(epoch FROM (projection.projected_at - projection.received_at)) * 1000)::bigint)
                   AS maximum_latency_ms_1h
          FROM repo_watch_webhook_projection AS projection
         WHERE projection.repository = selected.repository
           AND projection.projected_at >= transaction_timestamp() - interval '1 hour'
  ) AS latency_window ON true
  LEFT JOIN LATERAL (
        SELECT * FROM repo_watch_event
         WHERE repository = selected.repository
         ORDER BY cursor_generation DESC, event_ordinal DESC LIMIT 1
  ) AS observed ON true
  LEFT JOIN LATERAL (
        SELECT event.*
          FROM repo_watch_rule_evaluation AS evaluation
          JOIN repo_watch_event AS event ON event.event_id = evaluation.event_id
         WHERE evaluation.repository = selected.repository
           AND evaluation.outcome_kind <> 'not_matched'
         ORDER BY evaluation.cursor_generation DESC,
                  evaluation.event_ordinal DESC
         LIMIT 1
  ) AS actionable ON true
  LEFT JOIN LATERAL (
        SELECT batch.dispatch_id, batch.event_id, batch.rule_id, batch.admitted_at
          FROM repo_watch_dispatch_batch AS batch
         WHERE batch.repository = selected.repository
         ORDER BY batch.admitted_at DESC, batch.dispatch_id DESC LIMIT 1
  ) AS dispatch ON true
  LEFT JOIN LATERAL (
        SELECT dispatch_id, event_id, released_at
          FROM repo_watch_achieved_dispatch_settlement
         WHERE repository = selected.repository
         ORDER BY released_at DESC, dispatch_id DESC
         LIMIT 1
  ) AS settlement ON true
  LEFT JOIN repo_watch_current_repository_held_count AS held
    ON held.repository = selected.repository
  LEFT JOIN repo_watch_current_repository_obligation_count AS queued
    ON queued.repository = selected.repository
 ORDER BY selected.repository COLLATE "C"
"#;

async fn load_event_kind_counts(
    transaction: &mut Transaction<'_, Postgres>,
    repositories: &[String],
) -> Result<BTreeMap<String, Vec<RepoWatchEventKindCount>>, RepoWatchOperationsError> {
    if repositories.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows = sqlx::query(
        "SELECT repository, event_kind, count(*) AS event_count
           FROM repo_watch_event
          WHERE repository = ANY($1)
            AND recorded_at >= transaction_timestamp() - interval '1 hour'
          GROUP BY repository, event_kind
          ORDER BY repository, event_kind",
    )
    .bind(repositories)
    .fetch_all(&mut **transaction)
    .await?;
    let mut counts = BTreeMap::new();
    for row in rows {
        let repository: String = row.try_get("repository")?;
        counts
            .entry(repository)
            .or_insert_with(Vec::new)
            .push(RepoWatchEventKindCount {
                kind: decode_event_kind(&row.try_get::<String, _>("event_kind")?)?,
                count: nonnegative(row.try_get("event_count")?, "event kind count")?,
            });
    }
    Ok(counts)
}

fn decode_repository_status(
    row: &PgRow,
    event_kind_counts_previous_hour: Vec<RepoWatchEventKindCount>,
) -> Result<RepoWatchRepositoryStatus, RepoWatchOperationsError> {
    Ok(RepoWatchRepositoryStatus {
        repository: decode_repository(row.try_get("repository")?)?,
        cursor_generation: row
            .try_get::<Option<i64>, _>("generation")?
            .map(|generation| positive(generation, "cursor generation"))
            .transpose()?,
        observed_at: row
            .try_get::<Option<OffsetDateTime>, _>("recorded_at")?
            .map(decode_time),
        latest_webhook: row
            .try_get::<Option<i64>, _>("receipt_sequence")?
            .map(|sequence| {
                Ok::<_, RepoWatchOperationsError>(RepoWatchLatestWebhook {
                    receipt_sequence: positive(sequence, "webhook receipt sequence")?,
                    event_name: row.try_get("event_name")?,
                    action_name: row.try_get("action_name")?,
                    received_at: decode_time(row.try_get("received_at")?),
                })
            })
            .transpose()?,
        previous_five_minutes: decode_window(row, FIVE_MINUTES_SECONDS, "5m")?,
        previous_hour: decode_window(row, ONE_HOUR_SECONDS, "1h")?,
        latest_projection_latency_milliseconds: optional_nonnegative(
            row.try_get("latest_latency_ms")?,
            "latest projection latency",
        )?,
        maximum_projection_latency_milliseconds_previous_hour: optional_nonnegative(
            row.try_get("maximum_latency_ms_1h")?,
            "maximum projection latency",
        )?,
        event_kind_counts_previous_hour,
        last_observed_event: decode_prefixed_event(row, "observed")?,
        last_actionable_event: decode_prefixed_event(row, "actionable")?,
        last_dispatch_attempt: decode_dispatch(row, "dispatch")?,
        last_automation_settlement: decode_settlement(row, "settlement")?,
        held_slot_count: nonnegative(row.try_get("held_count")?, "held slot count")?,
        queued_obligation_count: nonnegative(
            row.try_get("queued_count")?,
            "queued obligation count",
        )?,
    })
}

fn decode_window(
    row: &PgRow,
    seconds: u32,
    suffix: &'static str,
) -> Result<RepoWatchWebhookWindow, RepoWatchOperationsError> {
    let received = match suffix {
        "5m" => row.try_get("received_5m")?,
        "1h" => row.try_get("received_1h")?,
        value => {
            return Err(RepoWatchOperationsCorruption::Unsupported {
                field: "webhook window",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let projected = match suffix {
        "5m" => row.try_get("projected_5m")?,
        "1h" => row.try_get("projected_1h")?,
        value => {
            return Err(RepoWatchOperationsCorruption::Unsupported {
                field: "webhook window",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let terminal = match suffix {
        "5m" => row.try_get("terminal_5m")?,
        "1h" => row.try_get("terminal_1h")?,
        value => {
            return Err(RepoWatchOperationsCorruption::Unsupported {
                field: "webhook window",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let quarantined = match suffix {
        "5m" => row.try_get("quarantined_5m")?,
        "1h" => row.try_get("quarantined_1h")?,
        value => {
            return Err(RepoWatchOperationsCorruption::Unsupported {
                field: "webhook window",
                value: value.to_owned(),
            }
            .into());
        }
    };
    Ok(RepoWatchWebhookWindow {
        seconds,
        received: nonnegative(received, "webhook received count")?,
        projected: nonnegative(projected, "webhook projected count")?,
        terminal: nonnegative(terminal, "webhook terminal count")?,
        quarantined: nonnegative(quarantined, "webhook quarantine count")?,
    })
}

fn decode_prefixed_event(
    row: &PgRow,
    prefix: &'static str,
) -> Result<Option<RepoWatchOperatorEvent>, RepoWatchOperationsError> {
    let fields = match prefix {
        "observed" => (
            "observed_event_id",
            "observed_generation",
            "observed_ordinal",
            "observed_kind",
            "observed_pull_request",
            "observed_at",
        ),
        "actionable" => (
            "actionable_event_id",
            "actionable_generation",
            "actionable_ordinal",
            "actionable_kind",
            "actionable_pull_request",
            "actionable_at",
        ),
        value => {
            return Err(RepoWatchOperationsCorruption::Unsupported {
                field: "event prefix",
                value: value.to_owned(),
            }
            .into());
        }
    };
    let Some(id) = row.try_get::<Option<Uuid>, _>(fields.0)? else {
        return Ok(None);
    };
    Ok(Some(RepoWatchOperatorEvent {
        id: RepoWatchEventId::from_uuid(id),
        cursor_generation: positive(row.try_get(fields.1)?, "event generation")?,
        event_ordinal: positive_u32(row.try_get(fields.2)?, "event ordinal")?,
        kind: decode_event_kind(&row.try_get::<String, _>(fields.3)?)?,
        pull_request: optional_pull_request(row.try_get(fields.4)?)?,
        observed_at: decode_time(row.try_get(fields.5)?),
    }))
}

fn decode_dispatch(
    row: &PgRow,
    prefix: &'static str,
) -> Result<Option<RepoWatchOperatorDispatch>, RepoWatchOperationsError> {
    if prefix != "dispatch" {
        return Err(RepoWatchOperationsCorruption::Unsupported {
            field: "dispatch prefix",
            value: prefix.to_owned(),
        }
        .into());
    }
    let Some(id) = row.try_get::<Option<Uuid>, _>("dispatch_id")? else {
        return Ok(None);
    };
    Ok(Some(RepoWatchOperatorDispatch {
        id: RepoWatchDispatchId::from_uuid(id),
        event: RepoWatchEventId::from_uuid(row.try_get("dispatch_event_id")?),
        rule: decode_rule(row.try_get("dispatch_rule_id")?)?,
        attempted_at: decode_time(row.try_get("dispatch_at")?),
    }))
}

fn decode_settlement(
    row: &PgRow,
    prefix: &'static str,
) -> Result<Option<RepoWatchOperatorSettlement>, RepoWatchOperationsError> {
    if prefix != "settlement" {
        return Err(RepoWatchOperationsCorruption::Unsupported {
            field: "settlement prefix",
            value: prefix.to_owned(),
        }
        .into());
    }
    let Some(id) = row.try_get::<Option<Uuid>, _>("settlement_dispatch_id")? else {
        return Ok(None);
    };
    Ok(Some(RepoWatchOperatorSettlement {
        dispatch: RepoWatchDispatchId::from_uuid(id),
        event: RepoWatchEventId::from_uuid(row.try_get("settlement_event_id")?),
        settled_at: decode_time(row.try_get("settlement_at")?),
    }))
}

fn decode_repository(value: String) -> Result<RepositorySlug, RepoWatchOperationsError> {
    RepositorySlug::try_new(value)
        .map_err(|_| RepoWatchOperationsCorruption::Invalid("repository").into())
}

fn decode_rule(value: String) -> Result<RepoWatchRuleId, RepoWatchOperationsError> {
    RepoWatchRuleId::try_new(value)
        .map_err(|_| RepoWatchOperationsCorruption::Invalid("rule").into())
}

fn decode_event_kind(value: &str) -> Result<RepoWatchEventKindNameV1, RepoWatchOperationsError> {
    repo_watch_event_kind_from_str(value).ok_or_else(|| {
        RepoWatchOperationsCorruption::Unsupported {
            field: "event kind",
            value: value.to_owned(),
        }
        .into()
    })
}

fn decode_time(value: OffsetDateTime) -> SystemTime {
    SystemTime::from(value)
}

fn positive(value: i64, field: &'static str) -> Result<u64, RepoWatchOperationsError> {
    u64::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| RepoWatchOperationsCorruption::Invalid(field).into())
}

fn nonnegative(value: i64, field: &'static str) -> Result<u64, RepoWatchOperationsError> {
    u64::try_from(value).map_err(|_| RepoWatchOperationsCorruption::Invalid(field).into())
}

fn optional_nonnegative(
    value: Option<i64>,
    field: &'static str,
) -> Result<Option<u64>, RepoWatchOperationsError> {
    value.map(|value| nonnegative(value, field)).transpose()
}

fn positive_u32(value: i32, field: &'static str) -> Result<u32, RepoWatchOperationsError> {
    u32::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| RepoWatchOperationsCorruption::Invalid(field).into())
}

fn optional_pull_request(
    value: Option<Decimal>,
) -> Result<Option<PullRequestNumber>, RepoWatchOperationsError> {
    value.map(pull_request).transpose()
}

fn pull_request(value: Decimal) -> Result<PullRequestNumber, RepoWatchOperationsError> {
    let value = positive_u64_from_numeric(value)
        .map_err(|_| RepoWatchOperationsCorruption::Invalid("pull request number"))?;
    Ok(PullRequestNumber::new(NonZeroU64::new(value).ok_or(
        RepoWatchOperationsCorruption::Invalid("pull request number"),
    )?))
}

fn decode_held_slot(row: &PgRow) -> Result<RepoWatchHeldSlot, RepoWatchOperationsError> {
    let blockers = row
        .try_get::<Vec<String>, _>("blockers")?
        .iter()
        .map(|blocker| match blocker.as_str() {
            "undelivered_action" => Ok(RepoWatchHeldSlotBlocker::UndeliveredAction),
            "delivery_turn_runtime_relevant" => {
                Ok(RepoWatchHeldSlotBlocker::DeliveryTurnRuntimeRelevant)
            }
            "live_runtime_turn" => Ok(RepoWatchHeldSlotBlocker::LiveRuntimeTurn),
            "pursuing_goal" => Ok(RepoWatchHeldSlotBlocker::PursuingGoal),
            value => Err(RepoWatchOperationsCorruption::Unsupported {
                field: "held slot blocker",
                value: value.to_owned(),
            }
            .into()),
        })
        .collect::<Result<Vec<_>, RepoWatchOperationsError>>()?;
    Ok(RepoWatchHeldSlot {
        dispatch: RepoWatchDispatchId::from_uuid(row.try_get("dispatch_id")?),
        singleton: decode_singleton(row)?,
        rule: decode_rule(row.try_get("rule_id")?)?,
        held_since: decode_time(row.try_get("held_since")?),
        sessions: row
            .try_get::<Vec<Uuid>, _>("session_ids")?
            .into_iter()
            .map(SessionId::from_uuid)
            .collect(),
        blockers,
    })
}

fn decode_obligation(row: &PgRow) -> Result<RepoWatchQueuedObligation, RepoWatchOperationsError> {
    let occupying = row.try_get::<Option<Uuid>, _>("occupying_dispatch_id")?;
    let parked_at = row.try_get::<Option<OffsetDateTime>, _>("parked_at")?;
    let eligible_at = row.try_get::<Option<OffsetDateTime>, _>("eligible_at")?;
    let eligibility_is_infinite = row.try_get::<bool, _>("eligibility_is_infinite")?;
    let ready = row.try_get::<bool, _>("ready")?;
    let externally_blocked = row.try_get::<bool, _>("externally_blocked")?;
    let occupying_sessions = || -> Result<Vec<SessionId>, sqlx::Error> {
        Ok(row
            .try_get::<Option<Vec<Uuid>>, _>("occupying_session_ids")?
            .unwrap_or_default()
            .into_iter()
            .map(SessionId::from_uuid)
            .collect())
    };
    let readiness = match (
        parked_at,
        occupying,
        externally_blocked,
        eligible_at,
        eligibility_is_infinite,
        ready,
    ) {
        (Some(parked_at), _, _, _, _, _) => RepoWatchObligationReadiness::Parked {
            parked_at: decode_time(parked_at),
        },
        (None, Some(dispatch), _, _, _, _) => RepoWatchObligationReadiness::Occupied {
            dispatch: RepoWatchDispatchId::from_uuid(dispatch),
            sessions: occupying_sessions()?,
        },
        (None, None, true, _, _, _) => RepoWatchObligationReadiness::ExternallyBlocked {
            sessions: occupying_sessions()?,
        },
        (None, None, false, eligible_at, eligibility_is_infinite, false)
            if eligible_at.is_some() || eligibility_is_infinite =>
        {
            RepoWatchObligationReadiness::Cooldown {
                eligible_at: eligible_at.map(decode_time),
            }
        }
        (None, None, false, _, _, true) => RepoWatchObligationReadiness::Ready,
        (None, None, false, _, _, false) => {
            return Err(RepoWatchOperationsCorruption::Invalid("obligation readiness").into());
        }
    };
    Ok(RepoWatchQueuedObligation {
        id: RepoWatchObligationId::from_uuid(row.try_get("obligation_id")?),
        singleton: decode_singleton(row)?,
        rule: decode_rule(row.try_get("rule_id")?)?,
        first_repository: decode_repository(row.try_get("first_repository")?)?,
        first_event: RepoWatchEventId::from_uuid(row.try_get("first_event_id")?),
        latest_event: RepoWatchEventId::from_uuid(row.try_get("latest_event_id")?),
        matched_event_count: positive(row.try_get("matched_event_count")?, "match count")?,
        owed_since: decode_time(row.try_get("owed_since")?),
        latest_match_at: decode_time(row.try_get("latest_match_at")?),
        failed_attempts: nonnegative(row.try_get("failed_attempts")?, "failed attempts")?,
        readiness,
    })
}

fn decode_pull_request_session(
    row: &PgRow,
    summaries: &mut BTreeMap<SessionId, signalbox_application::AttentionSummary>,
) -> Result<RepoWatchPullRequestSession, RepoWatchOperationsError> {
    let session = SessionId::from_uuid(row.try_get("session_id")?);
    let purpose_kind: String = row.try_get("purpose_kind")?;
    // The two arms read the same column from different tables: a rule dispatch
    // names repo_watch_dispatch_batch, an operator commission names
    // commissioned_dispatch. They are separate identity spaces, so each arm
    // wraps the value in its own newtype rather than sharing one.
    let dispatch: Uuid = row.try_get("dispatch_id")?;
    let template = row.try_get("template_name")?;
    let purpose = match purpose_kind.as_str() {
        "rule_dispatch" => RepoWatchSessionPurpose::RuleDispatch {
            dispatch: RepoWatchDispatchId::from_uuid(dispatch),
            event: RepoWatchEventId::from_uuid(row.try_get("event_id")?),
            rule: decode_rule(row.try_get("rule_id")?)?,
            template,
        },
        "operator_commission" => RepoWatchSessionPurpose::OperatorCommission {
            dispatch: CommissionedDispatchId::from_uuid(dispatch),
            template,
        },
        value => {
            return Err(RepoWatchOperationsCorruption::Unsupported {
                field: "session purpose",
                value: value.to_owned(),
            }
            .into());
        }
    };
    Ok(RepoWatchPullRequestSession {
        commissioned_at: decode_time(row.try_get("commissioned_at")?),
        purpose,
        attention: summaries
            .remove(&session)
            .ok_or(RepoWatchOperationsCorruption::Invalid(
                "session attention summary",
            ))?,
    })
}

fn decode_activity_event(row: &PgRow) -> Result<RepoWatchOperatorEvent, RepoWatchOperationsError> {
    Ok(RepoWatchOperatorEvent {
        id: RepoWatchEventId::from_uuid(row.try_get("event_id")?),
        cursor_generation: positive(row.try_get("cursor_generation")?, "event generation")?,
        event_ordinal: positive_u32(row.try_get("event_ordinal")?, "event ordinal")?,
        kind: decode_event_kind(&row.try_get::<String, _>("event_kind")?)?,
        pull_request: optional_pull_request(row.try_get("pull_request_number")?)?,
        observed_at: decode_time(row.try_get("recorded_at")?),
    })
}

fn decode_webhook_activity(
    row: &PgRow,
) -> Result<RepoWatchWebhookActivity, RepoWatchOperationsError> {
    Ok(RepoWatchWebhookActivity {
        receipt_sequence: positive(row.try_get("receipt_sequence")?, "receipt sequence")?,
        event_name: row.try_get("event_name")?,
        action_name: row.try_get("action_name")?,
        received_at: decode_time(row.try_get("received_at")?),
        projection_count: nonnegative(row.try_get("projection_count")?, "projection count")?,
        latest_projected_at: row
            .try_get::<Option<OffsetDateTime>, _>("latest_projected_at")?
            .map(decode_time),
        disposition: row
            .try_get::<Option<String>, _>("disposition")?
            .map(|value| decode_webhook_disposition(&value))
            .transpose()?,
    })
}

fn decode_webhook_disposition(
    value: &str,
) -> Result<RepoWatchWebhookDisposition, RepoWatchOperationsError> {
    let disposition = repo_watch_webhook_disposition_from_str(value).ok_or_else(|| {
        RepoWatchOperationsCorruption::Unsupported {
            field: "webhook disposition",
            value: value.to_owned(),
        }
    })?;
    Ok(match disposition {
        StoredWebhookDisposition::Projected => RepoWatchWebhookDisposition::Projected,
        StoredWebhookDisposition::Committed => RepoWatchWebhookDisposition::Committed,
        StoredWebhookDisposition::DuplicateState => RepoWatchWebhookDisposition::DuplicateState,
        StoredWebhookDisposition::Superseded => RepoWatchWebhookDisposition::Superseded,
        StoredWebhookDisposition::Ignored => RepoWatchWebhookDisposition::Ignored,
        StoredWebhookDisposition::Quarantined => RepoWatchWebhookDisposition::Quarantined,
    })
}

fn decode_singleton(row: &PgRow) -> Result<RepoWatchSingletonKey, RepoWatchOperationsError> {
    let scope = row.try_get::<String, _>("singleton_scope")?;
    let repository = row.try_get::<Option<String>, _>("singleton_repository")?;
    let pull_request = optional_pull_request(row.try_get("singleton_pull_request_number")?)?;
    let stack_root =
        optional_pull_request(row.try_get("singleton_stack_root_pull_request_number")?)?;
    match (scope.as_str(), repository, pull_request, stack_root) {
        ("pull_request", Some(repository), Some(number), None) => {
            Ok(RepoWatchSingletonKey::PullRequest {
                repository: RepositorySlug::try_new(repository)
                    .map_err(|_| RepoWatchOperationsCorruption::Invalid("singleton repository"))?,
                number,
            })
        }
        ("stack", Some(repository), None, Some(root_pull_request)) => {
            Ok(RepoWatchSingletonKey::Stack {
                repository: RepositorySlug::try_new(repository)
                    .map_err(|_| RepoWatchOperationsCorruption::Invalid("singleton repository"))?,
                root_pull_request,
            })
        }
        ("rule", None, None, None) => Ok(RepoWatchSingletonKey::Rule),
        ("repo", Some(repository), None, None) => Ok(RepoWatchSingletonKey::Repository {
            repository: RepositorySlug::try_new(repository)
                .map_err(|_| RepoWatchOperationsCorruption::Invalid("singleton repository"))?,
        }),
        _ => Err(RepoWatchOperationsCorruption::Invalid("singleton identity").into()),
    }
}

async fn load_pull_request_fact_rows(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    numbers: &[Decimal],
) -> Result<Vec<PgRow>, RepoWatchOperationsError> {
    if numbers.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query(PULL_REQUEST_FACTS_SQL)
        .bind(repository.as_str())
        .bind(numbers)
        .fetch_all(&mut **transaction)
        .await?)
}

const PULL_REQUEST_FACTS_SQL: &str = r#"
WITH selected AS (SELECT unnest($2::numeric[]) AS pull_request_number)
SELECT selected.pull_request_number,
       observed.event_id AS observed_event_id,
       observed.cursor_generation AS observed_generation,
       observed.event_ordinal AS observed_ordinal,
       observed.event_kind AS observed_kind,
       observed.pull_request_number AS observed_pull_request,
       observed.recorded_at AS observed_at,
       actionable.event_id AS actionable_event_id,
       actionable.cursor_generation AS actionable_generation,
       actionable.event_ordinal AS actionable_ordinal,
       actionable.event_kind AS actionable_kind,
       actionable.pull_request_number AS actionable_pull_request,
       actionable.recorded_at AS actionable_at,
       dispatch.dispatch_id, dispatch.event_id AS dispatch_event_id,
       dispatch.rule_id AS dispatch_rule_id, dispatch.admitted_at AS dispatch_at,
       settlement.dispatch_id AS settlement_dispatch_id,
       settlement.event_id AS settlement_event_id,
       settlement.released_at AS settlement_at,
       held.dispatch_id AS held_dispatch_id,
       queued.latest_event_id AS queued_latest_event_id,
       latest.dispatch_id AS latest_dispatch_id,
       latest.event_id AS latest_event_id,
       latest.head_sha AS latest_head_sha,
       latest.released_at AS latest_released_at,
       latest.achieved AS latest_achieved,
       COALESCE(counts.held_count, 0) AS held_count,
       COALESCE(counts.obligation_count, 0) AS queued_count,
       COALESCE(sessions.session_count, 0) AS session_count
  FROM selected
  LEFT JOIN LATERAL (
        SELECT * FROM repo_watch_event
         WHERE repository = $1 AND pull_request_number = selected.pull_request_number
         ORDER BY cursor_generation DESC, event_ordinal DESC LIMIT 1
  ) AS observed ON true
  LEFT JOIN LATERAL (
        SELECT event.*
          FROM repo_watch_rule_evaluation AS evaluation
          JOIN repo_watch_event AS event ON event.event_id = evaluation.event_id
         WHERE evaluation.repository = $1
           AND evaluation.pull_request_number = selected.pull_request_number
           AND evaluation.outcome_kind <> 'not_matched'
         ORDER BY evaluation.cursor_generation DESC,
                  evaluation.event_ordinal DESC
         LIMIT 1
  ) AS actionable ON true
  LEFT JOIN LATERAL (
        SELECT batch.dispatch_id, batch.event_id, batch.rule_id, batch.admitted_at
          FROM repo_watch_dispatch_batch AS batch
         WHERE batch.repository = $1
           AND batch.pull_request_number = selected.pull_request_number
         ORDER BY batch.admitted_at DESC, batch.dispatch_id DESC LIMIT 1
  ) AS dispatch ON true
  LEFT JOIN LATERAL (
        SELECT dispatch_id, event_id, released_at
          FROM repo_watch_achieved_dispatch_settlement
         WHERE repository = $1
           AND pull_request_number = selected.pull_request_number
         ORDER BY released_at DESC, dispatch_id DESC
         LIMIT 1
  ) AS settlement ON true
  LEFT JOIN LATERAL (
        SELECT dispatch_id FROM repo_watch_current_held_dispatch
         WHERE repository = $1 AND pull_request_number = selected.pull_request_number
         ORDER BY held_since DESC, dispatch_id DESC LIMIT 1
  ) AS held ON true
  LEFT JOIN LATERAL (
        SELECT obligation.latest_event_id
          FROM repo_watch_outstanding_dispatch_obligation AS obligation
          JOIN repo_watch_event AS latest_event
            ON latest_event.event_id = obligation.latest_event_id
         WHERE latest_event.repository = $1
           AND latest_event.pull_request_number = selected.pull_request_number
         ORDER BY obligation.latest_match_at DESC, obligation.obligation_id DESC LIMIT 1
  ) AS queued ON true
  LEFT JOIN LATERAL (
        SELECT batch.dispatch_id, batch.delivered_state_event_id AS event_id,
               delivered.head_sha, release.released_at,
               release.dispatch_id IS NOT NULL
               AND batch.delivered_state_event_id IS NOT NULL
               AND NOT EXISTS (
                    SELECT 1 FROM repo_watch_dispatch_action AS action
                     WHERE action.dispatch_id = batch.dispatch_id
                       AND NOT EXISTS (
                            SELECT 1
                              FROM repo_watch_dispatch_delivery AS delivery
                              JOIN goal_turn AS dispatched_turn
                                ON dispatched_turn.session_id = action.session_id
                               AND dispatched_turn.turn_id = delivery.turn_id
                              JOIN goal_event AS goal
                                ON goal.session_id = dispatched_turn.session_id
                               AND goal.generation = dispatched_turn.goal_generation
                               AND goal.event_ordinal = (
                                    SELECT max(candidate.event_ordinal)
                                      FROM goal_event AS candidate
                                     WHERE candidate.session_id = dispatched_turn.session_id
                                       AND candidate.generation = dispatched_turn.goal_generation
                               )
                               AND goal.event_kind = 'achieved'
                             WHERE delivery.dispatch_id = action.dispatch_id
                               AND delivery.action_ordinal = action.action_ordinal
                       )
               ) AS achieved
          FROM repo_watch_dispatch_batch AS batch
          LEFT JOIN repo_watch_event AS delivered
            ON delivered.event_id = batch.delivered_state_event_id
          LEFT JOIN repo_watch_dispatch_release AS release ON release.dispatch_id = batch.dispatch_id
         WHERE batch.repository = $1
           AND batch.pull_request_number = selected.pull_request_number
         ORDER BY batch.admitted_at DESC, batch.dispatch_id DESC LIMIT 1
  ) AS latest ON true
  LEFT JOIN repo_watch_current_pull_request_work_count AS counts
    ON counts.repository = $1
   AND counts.pull_request_number = selected.pull_request_number
  LEFT JOIN repo_watch_current_pull_request_session_count AS sessions
    ON sessions.repository = $1
   AND sessions.pull_request_number = selected.pull_request_number
 ORDER BY selected.pull_request_number
"#;

struct StoredPullRequestFacts {
    last_observed_event: Option<RepoWatchOperatorEvent>,
    last_actionable_event: Option<RepoWatchOperatorEvent>,
    last_dispatch_attempt: Option<RepoWatchOperatorDispatch>,
    last_automation_settlement: Option<RepoWatchOperatorSettlement>,
    held_dispatch: Option<RepoWatchDispatchId>,
    queued_event: Option<RepoWatchEventId>,
    latest_dispatch: Option<RepoWatchDispatchId>,
    latest_event: Option<RepoWatchEventId>,
    latest_head: Option<String>,
    latest_released_at: Option<SystemTime>,
    latest_achieved: bool,
    held_slot_count: u64,
    queued_obligation_count: u64,
    commissioned_session_count: u64,
}

impl StoredPullRequestFacts {
    const fn empty() -> Self {
        Self {
            last_observed_event: None,
            last_actionable_event: None,
            last_dispatch_attempt: None,
            last_automation_settlement: None,
            held_dispatch: None,
            queued_event: None,
            latest_dispatch: None,
            latest_event: None,
            latest_head: None,
            latest_released_at: None,
            latest_achieved: false,
            held_slot_count: 0,
            queued_obligation_count: 0,
            commissioned_session_count: 0,
        }
    }

    fn into_application(
        self,
        current_head: &str,
        open_parent: Option<PullRequestNumber>,
        open_child_count: u64,
    ) -> RepoWatchPullRequestOperationsFacts {
        let automation = match (
            self.held_dispatch,
            self.queued_event,
            self.latest_dispatch,
            self.latest_event,
            self.latest_released_at,
            self.latest_achieved,
        ) {
            (Some(dispatch), _, _, _, _, _) => RepoWatchAutomationStatus::Held { dispatch },
            (None, Some(latest_event), _, _, _, _) => {
                RepoWatchAutomationStatus::Queued { latest_event }
            }
            (None, None, None, _, _, _) => RepoWatchAutomationStatus::Unattempted,
            (None, None, Some(dispatch), _, _, false) => {
                RepoWatchAutomationStatus::NonConverged { dispatch }
            }
            (None, None, Some(dispatch), Some(sealed_event), Some(settled_at), true)
                if self.latest_head.as_deref() == Some(current_head) =>
            {
                RepoWatchAutomationStatus::CurrentHeadSealed {
                    dispatch,
                    sealed_event,
                    settled_at,
                }
            }
            (None, None, Some(dispatch), Some(sealed_event), Some(_), true) => {
                RepoWatchAutomationStatus::StaleSeal {
                    dispatch,
                    sealed_event,
                }
            }
            (None, None, Some(dispatch), _, _, true) => {
                RepoWatchAutomationStatus::NonConverged { dispatch }
            }
        };
        RepoWatchPullRequestOperationsFacts {
            open_parent,
            open_child_count,
            automation,
            last_observed_event: self.last_observed_event,
            last_actionable_event: self.last_actionable_event,
            last_dispatch_attempt: self.last_dispatch_attempt,
            last_automation_settlement: self.last_automation_settlement,
            held_slot_count: self.held_slot_count,
            queued_obligation_count: self.queued_obligation_count,
            commissioned_session_count: self.commissioned_session_count,
        }
    }
}

fn decode_pull_request_fact_row(
    row: &PgRow,
) -> Result<(u64, StoredPullRequestFacts), RepoWatchOperationsError> {
    let number = positive_u64_from_numeric(row.try_get("pull_request_number")?)
        .map_err(|_| RepoWatchOperationsCorruption::Invalid("pull request number"))?;
    Ok((
        number,
        StoredPullRequestFacts {
            last_observed_event: decode_prefixed_event(row, "observed")?,
            last_actionable_event: decode_prefixed_event(row, "actionable")?,
            last_dispatch_attempt: decode_dispatch(row, "dispatch")?,
            last_automation_settlement: decode_settlement(row, "settlement")?,
            held_dispatch: row
                .try_get::<Option<Uuid>, _>("held_dispatch_id")?
                .map(RepoWatchDispatchId::from_uuid),
            queued_event: row
                .try_get::<Option<Uuid>, _>("queued_latest_event_id")?
                .map(RepoWatchEventId::from_uuid),
            latest_dispatch: row
                .try_get::<Option<Uuid>, _>("latest_dispatch_id")?
                .map(RepoWatchDispatchId::from_uuid),
            latest_event: row
                .try_get::<Option<Uuid>, _>("latest_event_id")?
                .map(RepoWatchEventId::from_uuid),
            latest_head: row.try_get("latest_head_sha")?,
            latest_released_at: row
                .try_get::<Option<OffsetDateTime>, _>("latest_released_at")?
                .map(decode_time),
            latest_achieved: row
                .try_get::<Option<bool>, _>("latest_achieved")?
                .unwrap_or(false),
            held_slot_count: nonnegative(row.try_get("held_count")?, "held slot count")?,
            queued_obligation_count: nonnegative(
                row.try_get("queued_count")?,
                "queued obligation count",
            )?,
            commissioned_session_count: nonnegative(
                row.try_get("session_count")?,
                "session count",
            )?,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT_HEAD: &str = "1111111111111111111111111111111111111111";
    const STALE_HEAD: &str = "2222222222222222222222222222222222222222";

    fn achieved_facts(head: &str) -> StoredPullRequestFacts {
        StoredPullRequestFacts {
            latest_dispatch: Some(RepoWatchDispatchId::from_uuid(Uuid::from_u128(1))),
            latest_event: Some(RepoWatchEventId::from_uuid(Uuid::from_u128(2))),
            latest_head: Some(head.to_owned()),
            latest_released_at: Some(SystemTime::UNIX_EPOCH),
            latest_achieved: true,
            ..StoredPullRequestFacts::empty()
        }
    }

    fn expected_current_seal() -> RepoWatchAutomationStatus {
        RepoWatchAutomationStatus::CurrentHeadSealed {
            dispatch: RepoWatchDispatchId::from_uuid(Uuid::from_u128(1)),
            sealed_event: RepoWatchEventId::from_uuid(Uuid::from_u128(2)),
            settled_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn expected_stale_seal() -> RepoWatchAutomationStatus {
        RepoWatchAutomationStatus::StaleSeal {
            dispatch: RepoWatchDispatchId::from_uuid(Uuid::from_u128(1)),
            sealed_event: RepoWatchEventId::from_uuid(Uuid::from_u128(2)),
        }
    }

    #[test]
    fn automation_seal_is_bound_to_the_current_durable_head() {
        let current = achieved_facts(CURRENT_HEAD).into_application(CURRENT_HEAD, None, 0);
        let stale = achieved_facts(STALE_HEAD).into_application(CURRENT_HEAD, None, 0);

        assert_eq!(current.automation, expected_current_seal());
        assert_eq!(stale.automation, expected_stale_seal());
    }

    #[test]
    fn released_dispatch_without_achieved_goals_is_not_converged() {
        let facts = StoredPullRequestFacts {
            latest_dispatch: Some(RepoWatchDispatchId::from_uuid(Uuid::from_u128(3))),
            latest_event: Some(RepoWatchEventId::from_uuid(Uuid::from_u128(4))),
            latest_head: Some(CURRENT_HEAD.to_owned()),
            latest_released_at: Some(SystemTime::UNIX_EPOCH),
            latest_achieved: false,
            ..StoredPullRequestFacts::empty()
        };
        let projected = facts.into_application(CURRENT_HEAD, None, 0);

        assert_eq!(
            projected.automation,
            RepoWatchAutomationStatus::NonConverged {
                dispatch: RepoWatchDispatchId::from_uuid(Uuid::from_u128(3)),
            }
        );
    }
}
