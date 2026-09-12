//! Native review writes serialized with the repository's ingestion frontier.

use rust_decimal::Decimal;
use signalbox_session_ownership::{GitHubObjectId, RepositorySlug};
use sqlx::{Postgres, Transaction};

use crate::{RepoWatchStore, StoreError};

/// A native review mutation holding the repository's ingestion lock.
pub struct PendingReviewWrite {
    transaction: Transaction<'static, Postgres>,
    repository: RepositorySlug,
}

impl RepoWatchStore {
    /// Prevents ingestion from passing a mutation before its returned IDs are recorded.
    pub async fn begin_review_write(
        &self,
        repository: RepositorySlug,
    ) -> Result<PendingReviewWrite, StoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1, 0))")
            .bind(repository.as_str())
            .execute(&mut *transaction)
            .await?;
        Ok(PendingReviewWrite {
            transaction,
            repository,
        })
    }
}

impl PendingReviewWrite {
    /// Retains confirmed provider IDs and releases ingestion in the same commit.
    pub async fn record(
        mut self,
        review: GitHubObjectId,
        comment: Option<String>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO review_write_receipt(repository, review_id, comment_id)
             VALUES ($1,$2,$3) ON CONFLICT (repository, review_id) DO NOTHING",
        )
        .bind(self.repository.as_str())
        .bind(Decimal::from(review.get()))
        .bind(comment)
        .execute(&mut *self.transaction)
        .await?;
        self.transaction.commit().await?;
        Ok(())
    }
}
