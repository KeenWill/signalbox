//! Repository-watch v2's bounded module-local persistence surface.
//!
//! The crate has one Signalbox dependency: `signalbox-ownership-seam`. SQL is
//! unqualified and must run on a pool whose effective role and search path are
//! confined to `mod_repo_watch`.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_ownership_seam::{
    BranchName, CommitSha, OffsetDateTime, PullRequestBody, PullRequestNumber, PullRequestTitle,
    RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventContentIdentityV1,
    RepoWatchEventIdentityFrontierEntryV1, RepoWatchEventKindNameV1, RepoWatchEventTarget,
    RepoWatchRule, RepositorySlug,
};
use sqlx::PgPool;
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
    /// Either durable identity was already bound to different fact metadata.
    ConflictingReuse,
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
        })
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::InvalidProviderIdentity
            | Self::InvalidWebhookExpiry
            | Self::InvalidEventRetention => None,
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

    /// Upserts the durable occurrence counter for one normalized event stream.
    pub async fn upsert_frontier(
        &self,
        repository: &RepositorySlug,
        entry: RepoWatchEventIdentityFrontierEntryV1,
    ) -> Result<(), StoreError> {
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
        .execute(&self.pool)
        .await?;
        Ok(())
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

    /// Appends one normalized fact with its caller-selected retention boundary.
    pub async fn append_event(
        &self,
        event: &RepoWatchEvent,
        content_identity: RepoWatchEventContentIdentityV1,
        recorded_at: OffsetDateTime,
        retain_until: OffsetDateTime,
    ) -> Result<EventAdmission, StoreError> {
        if retain_until <= recorded_at {
            return Err(StoreError::InvalidEventRetention);
        }
        let (target_kind, pull_request_number) = match event.target() {
            RepoWatchEventTarget::PullRequest(context) => {
                ("pull_request", Some(Decimal::from(context.number().get())))
            }
            RepoWatchEventTarget::Branch => ("branch", None),
        };
        let inserted = sqlx::query(
            "INSERT INTO gh_event
                (event_id, content_identity, repository, event_kind, target_kind,
                 pull_request_number, recorded_at, retain_until)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT DO NOTHING",
        )
        .bind(event.id().into_uuid())
        .bind(content_identity.as_bytes().as_slice())
        .bind(event.repository().as_str())
        .bind(event_kind_storage(event.kind().name()))
        .bind(target_kind)
        .bind(pull_request_number)
        .bind(recorded_at)
        .bind(retain_until)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;
        if inserted {
            return Ok(EventAdmission::Inserted);
        }
        let exact: bool = sqlx::query_scalar(
            "SELECT COALESCE(
                count(*) = 1 AND bool_and(
                    event_id = $1
                    AND content_identity = $2
                    AND repository = $3
                    AND event_kind = $4
                    AND target_kind = $5
                    AND pull_request_number IS NOT DISTINCT FROM $6
                    AND recorded_at = $7
                    AND retain_until = $8
                ), false)
               FROM gh_event
              WHERE event_id = $1 OR content_identity = $2",
        )
        .bind(event.id().into_uuid())
        .bind(content_identity.as_bytes().as_slice())
        .bind(event.repository().as_str())
        .bind(event_kind_storage(event.kind().name()))
        .bind(target_kind)
        .bind(pull_request_number)
        .bind(recorded_at)
        .bind(retain_until)
        .fetch_one(&self.pool)
        .await?;
        Ok(if exact {
            EventAdmission::Replayed
        } else {
            EventAdmission::ConflictingReuse
        })
    }

    /// Activates one checked rule revision without retaining configuration text.
    pub async fn record_rule(
        &self,
        rule: &RepoWatchRule,
        activated_at: OffsetDateTime,
    ) -> Result<RuleAdmission, StoreError> {
        let revision = Decimal::from(rule.version().get());
        let digest = rule.content_digest();
        let mut transaction = self.pool.begin().await?;
        let active: Option<(Decimal, Vec<u8>)> = sqlx::query_as(
            "SELECT active_revision, content_digest
               FROM rule
              WHERE rule_id = $1
              FOR UPDATE",
        )
        .bind(rule.id().as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some((active_revision, active_digest)) = active else {
            sqlx::query(
                "INSERT INTO rule
                    (rule_id, active_revision, content_digest, updated_at)
                 VALUES ($1, $2, $3, statement_timestamp())",
            )
            .bind(rule.id().as_str())
            .bind(revision)
            .bind(digest.as_bytes().as_slice())
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO rule_revision
                    (rule_id, revision, content_digest, activated_at)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(rule.id().as_str())
            .bind(revision)
            .bind(digest.as_bytes().as_slice())
            .bind(activated_at)
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await?;
            return Ok(RuleAdmission::Inserted);
        };
        if revision < active_revision {
            transaction.rollback().await?;
            return Ok(RuleAdmission::Stale);
        }
        if revision == active_revision {
            transaction.rollback().await?;
            return Ok(if active_digest == digest.as_bytes() {
                RuleAdmission::Replayed
            } else {
                RuleAdmission::ConflictingReuse
            });
        }
        sqlx::query(
            "UPDATE rule_revision
                SET retired_at = GREATEST(activated_at, $3)
              WHERE rule_id = $1 AND revision = $2",
        )
        .bind(rule.id().as_str())
        .bind(active_revision)
        .bind(activated_at)
        .execute(&mut *transaction)
        .await?;
        let inserted = sqlx::query(
            "INSERT INTO rule_revision
                (rule_id, revision, content_digest, activated_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(rule.id().as_str())
        .bind(revision)
        .bind(digest.as_bytes().as_slice())
        .bind(activated_at)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if !inserted {
            let exact: bool = sqlx::query_scalar(
                "SELECT content_digest = $3 AND activated_at = $4
                   FROM rule_revision
                  WHERE rule_id = $1 AND revision = $2",
            )
            .bind(rule.id().as_str())
            .bind(revision)
            .bind(digest.as_bytes().as_slice())
            .bind(activated_at)
            .fetch_one(&mut *transaction)
            .await?;
            if !exact {
                transaction.rollback().await?;
                return Ok(RuleAdmission::ConflictingReuse);
            }
        }
        sqlx::query(
            "UPDATE rule
                SET active_revision = $2,
                    content_digest = $3,
                    updated_at = statement_timestamp()
              WHERE rule_id = $1",
        )
        .bind(rule.id().as_str())
        .bind(revision)
        .bind(digest.as_bytes().as_slice())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(RuleAdmission::Updated)
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
