//! Repository-watch v2's bounded module-local persistence surface.
//!
//! The crate has one Signalbox dependency: `signalbox-ownership-seam`. SQL is
//! unqualified and must run on a pool whose effective role and search path are
//! confined to `mod_repo_watch`.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use serde_json::{Value, json};
use signalbox_ownership_seam::{
    BranchName, CheckConclusion, ChecksOutcome, CommitSha, MergeableState, OffsetDateTime,
    PullRequestBody, PullRequestNumber, PullRequestTitle, ReactionChange, ReactionSubject,
    RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventIdentityFrontierV1,
    RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchEventOccurrenceV1,
    RepoWatchEventTarget, RepoWatchRule, RepositorySlug, ReviewState,
};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Current provider state for one watched repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryState<'a> {
    /// Canonical repository identity.
    pub repository: &'a RepositorySlug,
    /// Current default branch.
    pub default_branch: &'a BranchName,
    /// Current default-branch head.
    pub default_head: &'a CommitSha,
    /// When the complete observation was made.
    pub observed_at: OffsetDateTime,
}

/// Provider lifecycle retained for one current pull request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullRequestLifecycle {
    /// The pull request remains open.
    Open,
    /// It closed without merge.
    Closed,
    /// It merged.
    Merged,
}

impl PullRequestLifecycle {
    const fn storage(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }
}

/// Normalized current provider state for one pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestState<'a> {
    /// Watched repository.
    pub repository: &'a RepositorySlug,
    /// Positive provider number.
    pub number: PullRequestNumber,
    /// Provider lifecycle.
    pub lifecycle: PullRequestLifecycle,
    /// Current head revision.
    pub head: &'a CommitSha,
    /// Repository holding the head branch.
    pub head_repository: &'a RepositorySlug,
    /// Current head branch.
    pub head_branch: &'a BranchName,
    /// Current base branch.
    pub base_branch: &'a BranchName,
    /// Current title.
    pub title: &'a PullRequestTitle,
    /// Current body.
    pub body: &'a PullRequestBody,
    /// Whether GitHub marks it draft.
    pub draft: bool,
    /// Current author when GitHub retains the identity.
    pub author: Option<&'a RepoWatchAuthorLogin>,
    /// When the complete observation was made.
    pub observed_at: OffsetDateTime,
}

/// Authenticated webhook intake retained until its caller-selected expiry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookDelivery<'a> {
    /// Watched repository selected by the authenticated hook.
    pub repository: &'a RepositorySlug,
    /// Positive provider hook identity.
    pub hook_id: u64,
    /// Provider delivery identity.
    pub delivery_id: Uuid,
    /// GitHub event header.
    pub event: &'a str,
    /// Optional GitHub action member.
    pub action: Option<&'a str>,
    /// Digest of the exact authenticated bytes.
    pub body_digest: [u8; 32],
    /// Exact authenticated bytes.
    pub body: &'a [u8],
    /// Admission time.
    pub received_at: OffsetDateTime,
    /// Row-specific TTL boundary; no global retention window is selected.
    pub expires_at: OffsetDateTime,
}

/// Result of idempotently admitting a webhook delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookAdmission {
    /// The delivery was new.
    Inserted,
    /// The exact delivery was already retained.
    Replayed,
    /// The provider identity was already bound to different bytes or metadata.
    ConflictingReuse,
}

/// Terminal processing result for one admitted webhook.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookDisposition {
    /// The delivery updated module state.
    Applied,
    /// It was valid but produced no current-state change.
    Ignored,
    /// Its authenticated body could not form an admitted event.
    Rejected,
}

/// Result of idempotently recording one normalized GitHub fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventAdmission {
    /// The fact was new.
    Inserted,
    /// The exact fact was already retained.
    Replayed,
}

/// Result of atomically committing a complete frontier candidate and event batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrontierEventAdmission {
    /// The frontier and ordered facts committed together.
    Committed {
        /// Generation assigned to this complete candidate.
        generation: u64,
        /// Admission of each ordered fact.
        events: Box<[EventAdmission]>,
    },
    /// A durable event identity was already bound to a different fact.
    ConflictingReuse,
    /// A newer frontier entry already committed for this repository.
    Stale,
}

/// Result of activating one configured rule revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleAdmission {
    /// The rule was first observed.
    Inserted,
    /// A newer revision became active.
    Updated,
    /// The exact active revision was already retained.
    Replayed,
    /// The revision was already bound to different rule semantics.
    ConflictingReuse,
    /// The supplied revision predates the active revision.
    Stale,
}

impl WebhookDisposition {
    const fn storage(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Ignored => "ignored",
            Self::Rejected => "rejected",
        }
    }
}

/// Module-local storage failure.
#[derive(Debug)]
pub enum StoreError {
    /// PostgreSQL rejected or could not complete the operation.
    Database(sqlx::Error),
    /// A positive provider identity cannot fit the durable numeric shape.
    InvalidProviderIdentity,
    /// The webhook expiry does not follow its receipt time.
    InvalidWebhookExpiry,
    /// The event retention boundary does not follow its recording time.
    InvalidEventRetention,
    /// The supplied frontier generation has no successor.
    InvalidFrontierGeneration,
    /// A fact in a frontier commit belongs to another repository.
    EventRepositoryMismatch,
    /// The checked rule exposed too many identity fields for the durable inventory.
    InvalidRuleFieldInventory,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Database(_) => "repository-watch module database operation failed",
            Self::InvalidProviderIdentity => "repository-watch provider identity is not positive",
            Self::InvalidWebhookExpiry => "repository-watch webhook expiry is not after receipt",
            Self::InvalidEventRetention => {
                "repository-watch event retention is not after recording"
            }
            Self::InvalidFrontierGeneration => {
                "repository-watch frontier generation has no successor"
            }
            Self::EventRepositoryMismatch => {
                "repository-watch event does not belong to the frontier repository"
            }
            Self::InvalidRuleFieldInventory => {
                "repository-watch rule identity-field inventory is too large"
            }
        })
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::InvalidProviderIdentity
            | Self::InvalidWebhookExpiry
            | Self::InvalidEventRetention
            | Self::InvalidFrontierGeneration
            | Self::EventRepositoryMismatch
            | Self::InvalidRuleFieldInventory => None,
        }
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// SQL implementation over repository-watch's role-confined module pool.
#[derive(Clone, Debug)]
pub struct RepoWatchStore {
    pool: PgPool,
}

impl RepoWatchStore {
    /// Uses a pool already confined to the repository-watch role and schema.
    pub const fn new(module_pool: PgPool) -> Self {
        Self { pool: module_pool }
    }

    /// Replaces one repository's current provider projection.
    pub async fn upsert_repository(&self, state: RepositoryState<'_>) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO repository_state
                (repository, default_branch, default_head_sha, observed_at, updated_at)
             VALUES ($1, $2, $3, $4, statement_timestamp())
             ON CONFLICT (repository) DO UPDATE
             SET default_branch = EXCLUDED.default_branch,
                 default_head_sha = EXCLUDED.default_head_sha,
                 observed_at = EXCLUDED.observed_at,
                 updated_at = statement_timestamp()",
        )
        .bind(state.repository.as_str())
        .bind(state.default_branch.as_str())
        .bind(state.default_head.as_str())
        .bind(state.observed_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replaces one pull request's normalized current provider projection.
    pub async fn upsert_pull_request(&self, state: PullRequestState<'_>) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO pr_state
                (repository, pull_request_number, lifecycle, head_sha,
                 head_repository, head_branch, base_branch, title, body, draft,
                 author, observed_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                     statement_timestamp())
             ON CONFLICT (repository, pull_request_number) DO UPDATE
             SET lifecycle = EXCLUDED.lifecycle,
                 head_sha = EXCLUDED.head_sha,
                 head_repository = EXCLUDED.head_repository,
                 head_branch = EXCLUDED.head_branch,
                 base_branch = EXCLUDED.base_branch,
                 title = EXCLUDED.title,
                 body = EXCLUDED.body,
                 draft = EXCLUDED.draft,
                 author = EXCLUDED.author,
                 observed_at = EXCLUDED.observed_at,
                 updated_at = statement_timestamp()",
        )
        .bind(state.repository.as_str())
        .bind(Decimal::from(state.number.get()))
        .bind(state.lifecycle.storage())
        .bind(state.head.as_str())
        .bind(state.head_repository.as_str())
        .bind(state.head_branch.as_str())
        .bind(state.base_branch.as_str())
        .bind(state.title.as_str())
        .bind(state.body.as_str())
        .bind(state.draft)
        .bind(state.author.map(RepoWatchAuthorLogin::as_str))
        .bind(state.observed_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically admits the delivery metadata, body, and pending disposition.
    pub async fn admit_webhook(
        &self,
        delivery: WebhookDelivery<'_>,
    ) -> Result<WebhookAdmission, StoreError> {
        validate_webhook(&delivery)?;
        let hook_id = Decimal::from(delivery.hook_id);
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO webhook_delivery
                (hook_id, delivery_id, repository, event_kind, action,
                 body_digest, received_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT DO NOTHING",
        )
        .bind(hook_id)
        .bind(delivery.delivery_id)
        .bind(delivery.repository.as_str())
        .bind(delivery.event)
        .bind(delivery.action)
        .bind(delivery.body_digest.as_slice())
        .bind(delivery.received_at)
        .bind(delivery.expires_at)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if inserted {
            sqlx::query(
                "INSERT INTO webhook_body (hook_id, delivery_id, body)
                 VALUES ($1, $2, $3)",
            )
            .bind(hook_id)
            .bind(delivery.delivery_id)
            .bind(delivery.body)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO webhook_disposition (hook_id, delivery_id, disposition)
                 VALUES ($1, $2, 'pending')",
            )
            .bind(hook_id)
            .bind(delivery.delivery_id)
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await?;
            return Ok(WebhookAdmission::Inserted);
        }

        let equal: bool = sqlx::query_scalar(
            "SELECT delivery.repository = $3
                    AND delivery.event_kind = $4
                    AND delivery.action IS NOT DISTINCT FROM $5
                    AND delivery.body_digest = $6
                    AND body.body = $7
               FROM webhook_delivery AS delivery
               JOIN webhook_body AS body USING (hook_id, delivery_id)
              WHERE delivery.hook_id = $1 AND delivery.delivery_id = $2",
        )
        .bind(hook_id)
        .bind(delivery.delivery_id)
        .bind(delivery.repository.as_str())
        .bind(delivery.event)
        .bind(delivery.action)
        .bind(delivery.body_digest.as_slice())
        .bind(delivery.body)
        .fetch_one(&mut *transaction)
        .await?;
        transaction.rollback().await?;
        Ok(if equal {
            WebhookAdmission::Replayed
        } else {
            WebhookAdmission::ConflictingReuse
        })
    }

    /// Records one terminal disposition exactly once.
    pub async fn settle_webhook(
        &self,
        hook_id: u64,
        delivery_id: Uuid,
        disposition: WebhookDisposition,
        settled_at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        if hook_id == 0 {
            return Err(StoreError::InvalidProviderIdentity);
        }
        let updated = sqlx::query(
            "UPDATE webhook_disposition
                SET disposition = $3, settled_at = $4
              WHERE hook_id = $1 AND delivery_id = $2
                AND disposition = 'pending'",
        )
        .bind(Decimal::from(hook_id))
        .bind(delivery_id)
        .bind(disposition.storage())
        .bind(settled_at)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Advances module-local application from the prior visible core event.
    pub async fn advance_core_event(
        &self,
        prior_visible_sequence: u64,
        sequence: u64,
    ) -> Result<bool, StoreError> {
        let updated = sqlx::query(
            "UPDATE core_event_cursor
                SET applied_through = $1, updated_at = statement_timestamp()
              WHERE singleton
                AND applied_through = $2
                AND $1 > $2",
        )
        .bind(Decimal::from(sequence))
        .bind(Decimal::from(prior_visible_sequence))
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Atomically commits a complete frontier candidate and its ordered facts.
    pub async fn commit_frontier_candidate(
        &self,
        repository: &RepositorySlug,
        expected_generation: u64,
        frontier: &RepoWatchEventIdentityFrontierV1,
        events: &[RepoWatchEventOccurrenceV1],
        recorded_at: OffsetDateTime,
        retain_until: OffsetDateTime,
    ) -> Result<FrontierEventAdmission, StoreError> {
        if retain_until <= recorded_at {
            return Err(StoreError::InvalidEventRetention);
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1, 0))")
            .bind(repository.as_str())
            .execute(&mut *transaction)
            .await?;
        let frontier = frontier.entries().collect::<Vec<_>>();
        let candidate_identity = frontier_candidate_identity(&frontier, events);
        let current_generation: Decimal = sqlx::query_scalar(
            "SELECT frontier_generation FROM repository_state
              WHERE repository = $1 FOR UPDATE",
        )
        .bind(repository.as_str())
        .fetch_one(&mut *transaction)
        .await?;
        let expected = Decimal::from(expected_generation);
        if current_generation != expected {
            let exact_replay: bool = sqlx::query_scalar(
                "SELECT frontier_generation = $2 + 1
                        AND last_frontier_commit_digest = sha256($3)
                   FROM repository_state WHERE repository = $1",
            )
            .bind(repository.as_str())
            .bind(expected)
            .bind(candidate_identity.as_slice())
            .fetch_one(&mut *transaction)
            .await?;
            transaction.rollback().await?;
            return if exact_replay {
                Ok(FrontierEventAdmission::Committed {
                    generation: expected_generation
                        .checked_add(1)
                        .ok_or(StoreError::InvalidFrontierGeneration)?,
                    events: vec![EventAdmission::Replayed; events.len()].into_boxed_slice(),
                })
            } else {
                Ok(FrontierEventAdmission::Stale)
            };
        }
        let next_generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::InvalidFrontierGeneration)?;
        for entry in &frontier {
            let stale: Option<bool> = sqlx::query_scalar(
                "SELECT sequence > $3 FROM frontier
                  WHERE repository = $1 AND stream_identity = $2",
            )
            .bind(repository.as_str())
            .bind(entry.stream_identity().as_slice())
            .bind(Decimal::from(entry.sequence().get()))
            .fetch_optional(&mut *transaction)
            .await?;
            if stale.unwrap_or(false) {
                transaction.rollback().await?;
                return Ok(FrontierEventAdmission::Stale);
            }
        }
        let mut admissions = Vec::with_capacity(events.len());
        for occurrence in events {
            if occurrence.event().repository() != repository {
                return Err(StoreError::EventRepositoryMismatch);
            }
            match append_event(&mut transaction, occurrence, recorded_at, retain_until).await? {
                Some(admission) => admissions.push(admission),
                None => {
                    transaction.rollback().await?;
                    return Ok(FrontierEventAdmission::ConflictingReuse);
                }
            }
        }
        for entry in frontier {
            sqlx::query(
                "INSERT INTO frontier
                    (repository, stream_identity, sequence, pull_request_number, updated_at)
                 VALUES ($1, $2, $3, $4, statement_timestamp())
                 ON CONFLICT (repository, stream_identity) DO UPDATE
                 SET sequence = EXCLUDED.sequence,
                     pull_request_number = EXCLUDED.pull_request_number,
                     updated_at = statement_timestamp()",
            )
            .bind(repository.as_str())
            .bind(entry.stream_identity().as_slice())
            .bind(Decimal::from(entry.sequence().get()))
            .bind(
                entry
                    .pull_request_number()
                    .map(|number| Decimal::from(number.get())),
            )
            .execute(&mut *transaction)
            .await?;
        }
        let advanced = sqlx::query(
            "UPDATE repository_state
                SET frontier_generation = $2,
                    last_frontier_commit_digest = sha256($3),
                    updated_at = statement_timestamp()
              WHERE repository = $1 AND frontier_generation = $4",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(next_generation))
        .bind(candidate_identity.as_slice())
        .bind(expected)
        .execute(&mut *transaction)
        .await?;
        if advanced.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(FrontierEventAdmission::Stale);
        }
        transaction.commit().await?;
        Ok(FrontierEventAdmission::Committed {
            generation: next_generation,
            events: admissions.into_boxed_slice(),
        })
    }

    /// Releases one recurring stream after its rebuild subject has retired.
    pub async fn release_frontier(
        &self,
        repository: &RepositorySlug,
        stream_identity: &[u8; 32],
    ) -> Result<bool, StoreError> {
        let deleted = sqlx::query(
            "DELETE FROM frontier
              WHERE repository = $1 AND stream_identity = $2",
        )
        .bind(repository.as_str())
        .bind(stream_identity.as_slice())
        .execute(&self.pool)
        .await?;
        Ok(deleted.rows_affected() == 1)
    }

    /// Activates one checked rule revision without retaining configuration text.
    pub async fn record_rule(
        &self,
        repository: &RepositorySlug,
        rule: &RepoWatchRule,
        activated_at: OffsetDateTime,
    ) -> Result<RuleAdmission, StoreError> {
        let revision = Decimal::from(rule.version().get());
        let digest = rule.content_digest();
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(
                hashtextextended(length($1)::text || ':' || $1 || $2, 0))",
        )
        .bind(repository.as_str())
        .bind(rule.id().as_str())
        .execute(&mut *transaction)
        .await?;
        let active: Option<(Decimal, Vec<u8>)> = sqlx::query_as(
            "SELECT active_revision, content_digest
               FROM rule
              WHERE repository = $1 AND rule_id = $2
              FOR UPDATE",
        )
        .bind(repository.as_str())
        .bind(rule.id().as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        let latest_revision: Option<Decimal> = sqlx::query_scalar(
            "SELECT max(revision) FROM rule_revision
              WHERE repository = $1 AND rule_id = $2",
        )
        .bind(repository.as_str())
        .bind(rule.id().as_str())
        .fetch_one(&mut *transaction)
        .await?;
        let historical_digest: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT content_digest
               FROM rule_revision
              WHERE repository = $1 AND rule_id = $2 AND revision = $3",
        )
        .bind(repository.as_str())
        .bind(rule.id().as_str())
        .bind(revision)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(historical_digest) = historical_digest {
            transaction.rollback().await?;
            return Ok(if historical_digest != digest.as_bytes() {
                RuleAdmission::ConflictingReuse
            } else if active
                .as_ref()
                .is_some_and(|(active_revision, _)| *active_revision == revision)
            {
                RuleAdmission::Replayed
            } else {
                RuleAdmission::Stale
            });
        }
        if active
            .as_ref()
            .is_some_and(|(active_revision, _)| revision < *active_revision)
            || latest_revision.is_some_and(|latest_revision| revision < latest_revision)
        {
            transaction.rollback().await?;
            return Ok(RuleAdmission::Stale);
        }
        insert_rule_revision(&mut transaction, repository, rule, activated_at).await?;
        let admission = if let Some((active_revision, _)) = active {
            sqlx::query(
                "UPDATE rule_revision
                    SET retired_at = GREATEST(activated_at, $4)
                  WHERE repository = $1 AND rule_id = $2 AND revision = $3",
            )
            .bind(repository.as_str())
            .bind(rule.id().as_str())
            .bind(active_revision)
            .bind(activated_at)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE rule
                    SET active_revision = $3,
                        content_digest = $4,
                        updated_at = statement_timestamp()
                  WHERE repository = $1 AND rule_id = $2",
            )
            .bind(repository.as_str())
            .bind(rule.id().as_str())
            .bind(revision)
            .bind(digest.as_bytes().as_slice())
            .execute(&mut *transaction)
            .await?;
            RuleAdmission::Updated
        } else {
            sqlx::query(
                "INSERT INTO rule
                    (repository, rule_id, active_revision, content_digest, updated_at)
                 VALUES ($1, $2, $3, $4, statement_timestamp())",
            )
            .bind(repository.as_str())
            .bind(rule.id().as_str())
            .bind(revision)
            .bind(digest.as_bytes().as_slice())
            .execute(&mut *transaction)
            .await?;
            if latest_revision.is_some() {
                RuleAdmission::Updated
            } else {
                RuleAdmission::Inserted
            }
        };
        transaction.commit().await?;
        Ok(admission)
    }

    /// Deactivates one repository-scoped rule while retaining revision lineage.
    pub async fn deactivate_rule(
        &self,
        repository: &RepositorySlug,
        rule_id: &str,
        retired_at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(
                hashtextextended(length($1)::text || ':' || $1 || $2, 0))",
        )
        .bind(repository.as_str())
        .bind(rule_id)
        .execute(&mut *transaction)
        .await?;
        let active_revision: Option<Decimal> = sqlx::query_scalar(
            "SELECT active_revision FROM rule
              WHERE repository = $1 AND rule_id = $2 FOR UPDATE",
        )
        .bind(repository.as_str())
        .bind(rule_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(active_revision) = active_revision else {
            transaction.rollback().await?;
            return Ok(false);
        };
        sqlx::query(
            "UPDATE rule_revision
                SET retired_at = GREATEST(activated_at, $4)
              WHERE repository = $1 AND rule_id = $2 AND revision = $3",
        )
        .bind(repository.as_str())
        .bind(rule_id)
        .bind(active_revision)
        .bind(retired_at)
        .execute(&mut *transaction)
        .await?;
        sqlx::query("DELETE FROM rule WHERE repository = $1 AND rule_id = $2")
            .bind(repository.as_str())
            .bind(rule_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(true)
    }
}

fn frontier_candidate_identity(
    frontier: &[signalbox_ownership_seam::RepoWatchEventIdentityFrontierEntryV1],
    events: &[RepoWatchEventOccurrenceV1],
) -> Vec<u8> {
    let mut identity = b"signalbox-repo-watch-frontier-commit-v1".to_vec();
    for entry in frontier {
        identity.push(b'F');
        identity.extend_from_slice(entry.stream_identity().as_slice());
        identity.extend_from_slice(&entry.sequence().get().to_be_bytes());
        match entry.pull_request_number() {
            Some(number) => {
                identity.push(1);
                identity.extend_from_slice(&number.get().to_be_bytes());
            }
            None => identity.push(0),
        }
    }
    identity.push(b'E');
    for occurrence in events {
        identity.extend_from_slice(occurrence.content_identity().as_bytes());
    }
    identity
}

async fn append_event(
    transaction: &mut Transaction<'_, Postgres>,
    occurrence: &RepoWatchEventOccurrenceV1,
    recorded_at: OffsetDateTime,
    retain_until: OffsetDateTime,
) -> Result<Option<EventAdmission>, StoreError> {
    let event = occurrence.event();
    let (target_kind, pull_request_number) = match event.target() {
        RepoWatchEventTarget::PullRequest(context) => {
            ("pull_request", Some(Decimal::from(context.number().get())))
        }
        RepoWatchEventTarget::Branch => ("branch", None),
    };
    let payload = normalized_event_payload(event).to_string().into_bytes();
    let inserted = sqlx::query(
        "INSERT INTO gh_event
            (event_id, content_identity, repository, event_kind, target_kind,
             pull_request_number, normalized_payload, recorded_at, retain_until)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT DO NOTHING",
    )
    .bind(event.id().into_uuid())
    .bind(occurrence.content_identity().as_bytes().as_slice())
    .bind(event.repository().as_str())
    .bind(event_kind_storage(event.kind().name()))
    .bind(target_kind)
    .bind(pull_request_number)
    .bind(payload.as_slice())
    .bind(recorded_at)
    .bind(retain_until)
    .execute(&mut **transaction)
    .await?
    .rows_affected()
        == 1;
    if inserted {
        return Ok(Some(EventAdmission::Inserted));
    }
    let exact: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM gh_event WHERE content_identity = $2)
            AND NOT EXISTS (
                SELECT 1 FROM gh_event
                 WHERE event_id = $1 AND content_identity <> $2)",
    )
    .bind(event.id().into_uuid())
    .bind(occurrence.content_identity().as_bytes().as_slice())
    .fetch_one(&mut **transaction)
    .await?;
    Ok(exact.then_some(EventAdmission::Replayed))
}

async fn insert_rule_revision(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    rule: &RepoWatchRule,
    activated_at: OffsetDateTime,
) -> Result<(), StoreError> {
    let revision = Decimal::from(rule.version().get());
    let digest = rule.content_digest();
    sqlx::query(
        "INSERT INTO rule_revision
            (repository, rule_id, revision, content_digest, activated_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(repository.as_str())
    .bind(rule.id().as_str())
    .bind(revision)
    .bind(digest.as_bytes().as_slice())
    .bind(activated_at)
    .execute(&mut **transaction)
    .await?;
    for (index, (field, field_digest)) in rule.identity_field_digests().into_iter().enumerate() {
        let ordinal = i16::try_from(index).map_err(|_| StoreError::InvalidRuleFieldInventory)?;
        sqlx::query(
            "INSERT INTO rule_field_fingerprint
                (repository, rule_id, revision, field_ordinal, field_name, field_digest)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(repository.as_str())
        .bind(rule.id().as_str())
        .bind(revision)
        .bind(ordinal)
        .bind(field.configuration_path())
        .bind(field_digest.as_bytes().as_slice())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

fn normalized_event_payload(event: &RepoWatchEvent) -> Value {
    let target = match event.target() {
        RepoWatchEventTarget::PullRequest(context) => json!({
            "kind": "pull_request",
            "number": context.number().get(),
            "head_sha": context.head_sha().as_str(),
            "head_repository": context.head_repository().as_str(),
            "base_branch": context.base_branch().as_str(),
            "head_branch": context.head_branch().as_str(),
            "title": context.title().as_str(),
            "body": context.body().as_str(),
            "labels": context.labels().iter().map(|label| label.as_str()).collect::<Vec<_>>(),
            "draft": context.draft(),
            "author": context.author().map(RepoWatchAuthorLogin::as_str),
        }),
        RepoWatchEventTarget::Branch => json!({ "kind": "branch" }),
    };
    let kind = match event.kind() {
        RepoWatchEventKindV1::PullRequestOpened => json!({ "name": "pull_request_opened" }),
        RepoWatchEventKindV1::PullRequestClosed => json!({ "name": "pull_request_closed" }),
        RepoWatchEventKindV1::PullRequestMerged => json!({ "name": "pull_request_merged" }),
        RepoWatchEventKindV1::HeadChanged { previous, current } => json!({
            "name": "head_changed",
            "previous": previous.as_str(),
            "current": current.as_str(),
        }),
        RepoWatchEventKindV1::MergeableStateChanged { current } => json!({
            "name": "mergeable_state_changed",
            "current": mergeable_state_storage(*current),
        }),
        RepoWatchEventKindV1::ChecksCompleted { outcome } => json!({
            "name": "checks_completed",
            "outcome": checks_outcome_storage(*outcome),
        }),
        RepoWatchEventKindV1::CheckRunCompleted { name, conclusion } => json!({
            "name": "check_run_completed",
            "check_run": name.as_str(),
            "conclusion": check_conclusion_storage(*conclusion),
        }),
        RepoWatchEventKindV1::BranchWorkflowRunCompleted {
            branch,
            workflow,
            conclusion,
        } => json!({
            "name": "branch_workflow_run_completed",
            "branch": branch.as_str(),
            "workflow": workflow.as_str(),
            "conclusion": check_conclusion_storage(*conclusion),
        }),
        RepoWatchEventKindV1::ReviewSubmitted {
            reviewer,
            state,
            commit,
        } => json!({
            "name": "review_submitted",
            "reviewer": reviewer.as_str(),
            "state": review_state_storage(*state),
            "commit": commit.as_str(),
        }),
        RepoWatchEventKindV1::ThreadOpened { thread } => json!({
            "name": "thread_opened",
            "thread": thread.as_str(),
        }),
        RepoWatchEventKindV1::ThreadResolved { thread } => json!({
            "name": "thread_resolved",
            "thread": thread.as_str(),
        }),
        RepoWatchEventKindV1::Labeled { label } => json!({
            "name": "labeled",
            "label": label.as_str(),
        }),
        RepoWatchEventKindV1::Unlabeled { label } => json!({
            "name": "unlabeled",
            "label": label.as_str(),
        }),
        RepoWatchEventKindV1::BaseAdvanced { branch } => json!({
            "name": "base_advanced",
            "branch": branch.as_str(),
        }),
        RepoWatchEventKindV1::ReactionChanged {
            subject,
            reactor,
            content,
            change,
        } => json!({
            "name": "reaction_changed",
            "subject": reaction_subject_payload(*subject),
            "reactor": reactor.as_str(),
            "content": content.as_str(),
            "change": reaction_change_storage(*change),
        }),
    };
    json!({
        "repository": event.repository().as_str(),
        "target": target,
        "kind": kind,
    })
}

const fn mergeable_state_storage(value: MergeableState) -> &'static str {
    match value {
        MergeableState::Mergeable => "mergeable",
        MergeableState::Conflicting => "conflicting",
        MergeableState::Unknown => "unknown",
    }
}

const fn checks_outcome_storage(value: ChecksOutcome) -> &'static str {
    match value {
        ChecksOutcome::Success => "success",
        ChecksOutcome::Failure => "failure",
    }
}

const fn check_conclusion_storage(value: CheckConclusion) -> &'static str {
    match value {
        CheckConclusion::Success => "success",
        CheckConclusion::Failure => "failure",
        CheckConclusion::Neutral => "neutral",
        CheckConclusion::Cancelled => "cancelled",
        CheckConclusion::Skipped => "skipped",
        CheckConclusion::TimedOut => "timed_out",
        CheckConclusion::ActionRequired => "action_required",
        CheckConclusion::Stale => "stale",
        CheckConclusion::StartupFailure => "startup_failure",
    }
}

const fn review_state_storage(value: ReviewState) -> &'static str {
    match value {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes_requested",
        ReviewState::Commented => "commented",
    }
}

fn reaction_subject_payload(value: ReactionSubject) -> Value {
    match value {
        ReactionSubject::PullRequestBody => json!({ "kind": "pull_request_body" }),
        ReactionSubject::IssueComment { id } => {
            json!({ "kind": "issue_comment", "id": id.get() })
        }
        ReactionSubject::ReviewComment { id } => {
            json!({ "kind": "review_comment", "id": id.get() })
        }
    }
}

const fn reaction_change_storage(value: ReactionChange) -> &'static str {
    match value {
        ReactionChange::Added => "added",
        ReactionChange::Removed => "removed",
    }
}

/// Returns configured rules whose checked matcher accepts one normalized fact.
pub fn matching_rules<'a>(
    rules: &'a [RepoWatchRule],
    event: &RepoWatchEvent,
) -> Vec<&'a RepoWatchRule> {
    rules
        .iter()
        .filter(|rule| rule.matcher().matches(event))
        .collect()
}

const fn event_kind_storage(kind: RepoWatchEventKindNameV1) -> &'static str {
    match kind {
        RepoWatchEventKindNameV1::PullRequestOpened => "pull_request_opened",
        RepoWatchEventKindNameV1::PullRequestClosed => "pull_request_closed",
        RepoWatchEventKindNameV1::PullRequestMerged => "pull_request_merged",
        RepoWatchEventKindNameV1::HeadChanged => "head_changed",
        RepoWatchEventKindNameV1::MergeableStateChanged => "mergeable_state_changed",
        RepoWatchEventKindNameV1::ChecksCompleted => "checks_completed",
        RepoWatchEventKindNameV1::CheckRunCompleted => "check_run_completed",
        RepoWatchEventKindNameV1::BranchWorkflowRunCompleted => "branch_workflow_run_completed",
        RepoWatchEventKindNameV1::ReviewSubmitted => "review_submitted",
        RepoWatchEventKindNameV1::ThreadOpened => "thread_opened",
        RepoWatchEventKindNameV1::ThreadResolved => "thread_resolved",
        RepoWatchEventKindNameV1::Labeled => "labeled",
        RepoWatchEventKindNameV1::Unlabeled => "unlabeled",
        RepoWatchEventKindNameV1::BaseAdvanced => "base_advanced",
        RepoWatchEventKindNameV1::ReactionChanged => "reaction_changed",
    }
}

fn validate_webhook(delivery: &WebhookDelivery<'_>) -> Result<(), StoreError> {
    if delivery.hook_id == 0 {
        Err(StoreError::InvalidProviderIdentity)
    } else if delivery.expires_at <= delivery.received_at {
        Err(StoreError::InvalidWebhookExpiry)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{StoreError, WebhookDelivery, validate_webhook};
    use signalbox_ownership_seam::{OffsetDateTime, RepositorySlug};
    use uuid::Uuid;

    #[test]
    fn webhook_expiry_validation_names_the_only_module_owned_ttl_boundary() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let repository = RepositorySlug::try_new(String::from("owner/repository"))
            .expect("fixture repository is valid");
        let input = WebhookDelivery {
            repository: &repository,
            hook_id: 1,
            delivery_id: Uuid::from_u128(2),
            event: "pull_request",
            action: Some("opened"),
            body_digest: [3; 32],
            body: b"{}",
            received_at: now,
            expires_at: now,
        };
        assert!(matches!(
            validate_webhook(&input),
            Err(StoreError::InvalidWebhookExpiry)
        ));
    }
}
