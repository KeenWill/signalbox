//! Repository-watch v2's bounded module-local persistence surface.
//!
//! The crate has one Signalbox dependency: `signalbox-ownership-seam`. SQL is
//! unqualified and must run on a pool whose effective role and search path are
//! confined to `mod_repo_watch`.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_ownership_seam::{
    BranchName, CommitSha, OffsetDateTime, PullRequestBody, PullRequestNumber, PullRequestTitle,
    RepoWatchAuthorLogin, RepositorySlug,
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
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Database(_) => "repository-watch module database operation failed",
            Self::InvalidProviderIdentity => "repository-watch provider identity is not positive",
            Self::InvalidWebhookExpiry => "repository-watch webhook expiry is not after receipt",
        })
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::InvalidProviderIdentity | Self::InvalidWebhookExpiry => None,
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
