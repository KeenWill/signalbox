//! Confirmed GitHub review-write identities retained by the daemon.

use std::{fmt::Debug, num::NonZeroU64};

use futures_util::future::BoxFuture;

/// Failure to acquire or retain a review-write receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewWriteError;

/// Serializes a review mutation with ingestion for its repository.
pub trait ReviewWriteRecorder: Debug + Send + Sync {
    /// Holds ingestion until the mutation's returned identities are retained or abandoned.
    fn begin<'a>(
        &'a self,
        repository: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn PendingReviewWrite>, ReviewWriteError>>;
}

/// A mutation whose response has not yet been retained.
pub trait PendingReviewWrite: Send {
    /// Retains the returned review and optional reply ID, then releases ingestion.
    fn record(
        self: Box<Self>,
        review: NonZeroU64,
        comment: Option<String>,
    ) -> BoxFuture<'static, Result<(), ReviewWriteError>>;
}
