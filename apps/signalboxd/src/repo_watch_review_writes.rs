//! Composition between native GitHub writes and the module-confined receipt store.

use std::{num::NonZeroU64, sync::OnceLock};

use futures_util::future::BoxFuture;
use signalbox_domain::{GitHubObjectId, RepositorySlug};
use signalbox_github_transport::{PendingReviewWrite, ReviewWriteError, ReviewWriteRecorder};
use signalbox_module_repo_watch_v2::RepoWatchStore;

/// Receives the module store before startup admits tool execution.
#[derive(Debug, Default)]
pub struct RepositoryReviewWriteRecorder {
    store: OnceLock<RepoWatchStore>,
}

impl RepositoryReviewWriteRecorder {
    /// Installs the independently authenticated module store once during startup.
    pub fn initialize(&self, store: RepoWatchStore) -> Result<(), ReviewWriteError> {
        self.store.set(store).map_err(|_| ReviewWriteError)
    }
}

struct Pending(signalbox_module_repo_watch_v2::PendingReviewWrite);

impl ReviewWriteRecorder for RepositoryReviewWriteRecorder {
    fn begin<'a>(
        &'a self,
        repository: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn PendingReviewWrite>, ReviewWriteError>> {
        Box::pin(async move {
            let store = self.store.get().ok_or(ReviewWriteError)?;
            let repository =
                RepositorySlug::try_new(repository.to_owned()).map_err(|_| ReviewWriteError)?;
            let pending = store
                .begin_review_write(repository)
                .await
                .map_err(|_| ReviewWriteError)?;
            Ok(Box::new(Pending(pending)) as Box<dyn PendingReviewWrite>)
        })
    }
}

impl PendingReviewWrite for Pending {
    fn record(
        self: Box<Self>,
        review: NonZeroU64,
        comment: Option<String>,
    ) -> BoxFuture<'static, Result<(), ReviewWriteError>> {
        Box::pin(async move {
            let review = GitHubObjectId::new(review);
            self.0
                .record(review, comment)
                .await
                .map_err(|_| ReviewWriteError)
        })
    }
}
