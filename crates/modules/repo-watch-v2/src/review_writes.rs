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
    /// Pauses repository event evaluation until this observer's identity is resolved.
    pub async fn prepare_observer_identity(
        &self,
        repository: &RepositorySlug,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1,0))")
            .bind(repository.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO observer_actor(repository) VALUES ($1) ON CONFLICT(repository) DO UPDATE SET ready=false")
            .bind(repository.as_str()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn observer_evaluation_paused(
        &self,
        repository: &RepositorySlug,
    ) -> Result<bool, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM observer_actor WHERE repository=$1 AND NOT ready)",
        )
        .bind(repository.as_str())
        .fetch_one(&self.pool)
        .await?)
    }

    pub(crate) async fn record_observer_identity(
        &self,
        repository: &RepositorySlug,
        actor: &signalbox_session_ownership::RepoWatchAuthorLogin,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1,0))")
            .bind(repository.as_str())
            .execute(&mut *tx)
            .await?;
        let previous: Option<Option<String>> =
            sqlx::query_scalar("SELECT login FROM observer_actor WHERE repository=$1 FOR UPDATE")
                .bind(repository.as_str())
                .fetch_optional(&mut *tx)
                .await?;
        if previous.flatten().is_none() {
            sqlx::query("UPDATE gh_event SET self_review=true WHERE repository=$1 AND source_review_actor=$2")
                .bind(repository.as_str()).bind(actor.as_str()).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO observer_actor(repository,login,ready) VALUES ($1,$2,true) ON CONFLICT(repository) DO UPDATE SET login=EXCLUDED.login,ready=true")
            .bind(repository.as_str()).bind(actor.as_str()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

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
