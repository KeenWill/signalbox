//! Attachment replica verification before durable model-call send authorization.

use std::{error::Error, fmt, future::Future, num::NonZeroU64, sync::Arc, time::Duration};

use signalbox_application::{
    AttachmentPreparationFailure, ClassifyOperatorFailure, ModelCallCapabilityPreparation,
    ModelCallProvider, OperatorFailureClass, PreparedModelOperation, relinquish_scheduler_capacity,
};
use signalbox_blob_store::{BlobObjectKey, BlobStore, BlobStoreFailureKind, ExpectedBlob};
use signalbox_domain::{
    AuthorizedModelCall, BlobDigest, CorrelatedModelCallTerminalObservation,
    PreparedModelCallRequest,
};
use signalbox_persistence::blob::{BlobCatalogRepository, BlobCatalogRepositoryError};
use sqlx::PgPool;
use tokio::{
    sync::Semaphore,
    time::{Instant, timeout_at},
};

use crate::BlobStoreRegistry;

const MAX_ACTIVE_ATTACHMENT_PREPARATIONS: usize = 8;
const ATTACHMENT_PREPARATION_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);

/// Sanitized failure from attachment verification or the wrapped provider.
#[derive(Debug)]
pub enum AttachmentPreparingProviderError<E> {
    /// The wrapped provider failed after all attachments verified.
    Provider(E),
    /// Durable catalog and configured store facts disagreed.
    Integrity,
}

impl<E> fmt::Display for AttachmentPreparingProviderError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider(error) => error.fmt(formatter),
            Self::Integrity => formatter.write_str("attachment preparation failed closed"),
        }
    }
}

impl<E> Error for AttachmentPreparingProviderError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Provider(error) => Some(error),
            Self::Integrity => None,
        }
    }
}

impl<E> ClassifyOperatorFailure for AttachmentPreparingProviderError<E>
where
    E: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Provider(error) => error.operator_failure_class(),
            Self::Integrity => OperatorFailureClass::FailClosedCorruption,
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Provider(error) => error.operator_failure_cause_code(),
            Self::Integrity => "attachment_preparation_integrity",
        }
    }
}

/// Provider wrapper that verifies every distinct rendered attachment first.
#[derive(Clone, Debug)]
pub struct AttachmentPreparingModelCallProvider<P> {
    inner: P,
    repository: BlobCatalogRepository,
    registry: Option<Arc<BlobStoreRegistry>>,
    permits: Arc<Semaphore>,
}

impl<P> AttachmentPreparingModelCallProvider<P> {
    /// Binds attachment preparation to the durable catalog and configured stores.
    pub fn new(inner: P, pool: PgPool, registry: Option<Arc<BlobStoreRegistry>>) -> Self {
        Self {
            inner,
            repository: BlobCatalogRepository::new(pool),
            registry,
            permits: Arc::new(Semaphore::new(MAX_ACTIVE_ATTACHMENT_PREPARATIONS)),
        }
    }
}

impl<P> ModelCallProvider for AttachmentPreparingModelCallProvider<P>
where
    P: ModelCallProvider + Send,
    P::Capability: Send,
{
    type Capability = P::Capability;
    type Error = AttachmentPreparingProviderError<P::Error>;

    async fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        if operation.request().attachment_blobs().next().is_none() {
            return self
                .inner
                .prepare_capability(operation, cancellation)
                .await
                .map_err(AttachmentPreparingProviderError::Provider);
        }
        let registry = self
            .registry
            .as_ref()
            .ok_or(AttachmentPreparingProviderError::Integrity)?;
        if attachment_lengths_exceed(
            operation.request().attachment_blobs(),
            registry.max_blob_bytes(),
        ) {
            return Ok(ModelCallCapabilityPreparation::AttachmentKnownFailure(
                AttachmentPreparationFailure::TooLarge {
                    maximum_bytes: registry.max_blob_bytes(),
                },
            ));
        }
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            return Ok(ModelCallCapabilityPreparation::Deferred);
        };
        let deadline = Instant::now() + ATTACHMENT_PREPARATION_DEADLINE;
        let mut cancellation = Box::pin(cancellation);
        let verification = async {
            tokio::select! {
                biased;
                () = cancellation.as_mut() => None,
                outcome = verify_attachments(
                    &self.repository,
                    registry,
                    operation.request(),
                    deadline,
                ) => Some(outcome),
            }
        };
        let outcome = relinquish_scheduler_capacity(verification).await;
        drop(permit);
        let Some(outcome) = outcome else {
            return Ok(ModelCallCapabilityPreparation::Cancelled);
        };
        match outcome {
            Ok(()) => self
                .inner
                .prepare_capability(operation, cancellation)
                .await
                .map_err(AttachmentPreparingProviderError::Provider),
            Err(AttachmentVerificationFailure::Known(failure)) => Ok(
                ModelCallCapabilityPreparation::AttachmentKnownFailure(failure),
            ),
            Err(AttachmentVerificationFailure::Unavailable) => {
                Ok(ModelCallCapabilityPreparation::AttachmentUnavailable(
                    AttachmentPreparationFailure::Unavailable,
                ))
            }
            Err(AttachmentVerificationFailure::Integrity) => {
                Err(AttachmentPreparingProviderError::Integrity)
            }
        }
    }

    async fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> Result<CorrelatedModelCallTerminalObservation, Self::Error>
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        self.inner
            .invoke(authorized, capability, acceptance_possible, cancellation)
            .await
            .map_err(AttachmentPreparingProviderError::Provider)
    }
}

fn attachment_lengths_exceed(
    mut attachments: impl Iterator<Item = (BlobDigest, NonZeroU64)>,
    maximum: u64,
) -> bool {
    attachments
        .try_fold(0_u64, |total, (_, length)| {
            total
                .checked_add(length.get())
                .filter(|total| *total <= maximum)
        })
        .is_none()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttachmentVerificationFailure {
    Known(AttachmentPreparationFailure),
    Unavailable,
    Integrity,
}

async fn verify_attachments(
    repository: &BlobCatalogRepository,
    registry: &BlobStoreRegistry,
    request: &PreparedModelCallRequest,
    deadline: Instant,
) -> Result<(), AttachmentVerificationFailure> {
    for (digest, byte_length) in request.attachment_blobs() {
        let expected = ExpectedBlob::new(digest, byte_length);
        let entry = timeout_at(deadline, repository.find(digest))
            .await
            .map_err(|_| AttachmentVerificationFailure::Unavailable)?
            .map_err(map_catalog_error)?
            .ok_or(AttachmentVerificationFailure::Known(
                AttachmentPreparationFailure::Missing { digest },
            ))?;
        if entry.expected() != expected {
            return Err(AttachmentVerificationFailure::Integrity);
        }
        let candidates = entry
            .replicas()
            .iter()
            .map(|replica| {
                registry
                    .recorded_store(replica.store())
                    .map(|store| (store, replica.object_key().clone()))
                    .ok_or(AttachmentVerificationFailure::Integrity)
            })
            .collect::<Result<Vec<_>, _>>()?;
        match verify_replica_candidates(expected, &candidates, deadline).await {
            ReplicaVerification::Verified => {}
            ReplicaVerification::Missing => {
                return Err(AttachmentVerificationFailure::Known(
                    AttachmentPreparationFailure::Missing { digest },
                ));
            }
            ReplicaVerification::Corrupt => {
                return Err(AttachmentVerificationFailure::Known(
                    AttachmentPreparationFailure::Corrupt { digest },
                ));
            }
            ReplicaVerification::Unavailable => {
                return Err(AttachmentVerificationFailure::Unavailable);
            }
        }
    }
    Ok(())
}

fn map_catalog_error(error: BlobCatalogRepositoryError) -> AttachmentVerificationFailure {
    match error {
        BlobCatalogRepositoryError::Database(_)
        | BlobCatalogRepositoryError::CommitAmbiguous(_) => {
            AttachmentVerificationFailure::Unavailable
        }
        BlobCatalogRepositoryError::Corruption(_) => AttachmentVerificationFailure::Integrity,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplicaVerification {
    Verified,
    Missing,
    Corrupt,
    Unavailable,
}

async fn verify_replica_candidates(
    expected: ExpectedBlob,
    candidates: &[(Arc<dyn BlobStore>, BlobObjectKey)],
    deadline: Instant,
) -> ReplicaVerification {
    if candidates.is_empty() {
        return ReplicaVerification::Missing;
    }
    let mut saw_missing = false;
    let mut saw_corrupt = false;
    let mut saw_unavailable = false;
    for (store, key) in candidates {
        match timeout_at(
            deadline,
            store.open_range(expected, key, 0, NonZeroU64::MIN),
        )
        .await
        {
            Ok(Ok(opened)) if opened.byte_length() == 1 => return ReplicaVerification::Verified,
            Ok(Ok(_)) => saw_unavailable = true,
            Ok(Err(error)) => match error.kind() {
                BlobStoreFailureKind::NotFound => saw_missing = true,
                BlobStoreFailureKind::VerificationFailed => saw_corrupt = true,
                BlobStoreFailureKind::PublicationAmbiguous | BlobStoreFailureKind::Unavailable => {
                    saw_unavailable = true
                }
            },
            Err(_) => saw_unavailable = true,
        }
    }
    if saw_unavailable {
        ReplicaVerification::Unavailable
    } else if saw_corrupt {
        ReplicaVerification::Corrupt
    } else if saw_missing {
        ReplicaVerification::Missing
    } else {
        ReplicaVerification::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_blob_store::{
        BlobPutOutcome, BlobReader, BlobStoreError, BlobStoreFuture, BlobVerificationFailure,
        OpenedBlob,
    };

    #[derive(Clone, Copy)]
    enum StoreOutcome {
        Missing,
        Corrupt,
        PublicationAmbiguous,
        Unavailable,
    }

    struct FakeStore(StoreOutcome);

    impl BlobStore for FakeStore {
        fn put<'a>(
            &'a self,
            _expected: ExpectedBlob,
            _source: BlobReader,
        ) -> BlobStoreFuture<'a, BlobPutOutcome> {
            Box::pin(async { Err(BlobStoreError::unavailable("unused_test_put")) })
        }

        fn open<'a>(&'a self, _key: &'a BlobObjectKey) -> BlobStoreFuture<'a, OpenedBlob> {
            Box::pin(async { Err(BlobStoreError::unavailable("unused_test_open")) })
        }

        fn open_range<'a>(
            &'a self,
            expected: ExpectedBlob,
            _key: &'a BlobObjectKey,
            _offset: u64,
            _byte_length: NonZeroU64,
        ) -> BlobStoreFuture<'a, OpenedBlob> {
            Box::pin(async move {
                match self.0 {
                    StoreOutcome::Missing => Err(BlobStoreError::not_found("test_open_range")),
                    StoreOutcome::Corrupt => Err(BlobStoreError::verification(
                        "test_open_range",
                        BlobVerificationFailure::new(expected, None, expected.byte_length()),
                    )),
                    StoreOutcome::PublicationAmbiguous => {
                        Err(BlobStoreError::publication_ambiguous("test_open_range"))
                    }
                    StoreOutcome::Unavailable => {
                        Err(BlobStoreError::unavailable("test_open_range"))
                    }
                }
            })
        }
    }

    fn digest(byte: u8) -> BlobDigest {
        BlobDigest::from_bytes([byte; 32])
    }

    fn key() -> BlobObjectKey {
        BlobObjectKey::for_digest(digest(1))
    }

    #[tokio::test]
    async fn inv062_missing_replica_set_is_typed_missing() {
        let expected = ExpectedBlob::new(digest(1), NonZeroU64::MIN);
        let deadline = Instant::now() + Duration::from_secs(1);
        let missing = [(
            Arc::new(FakeStore(StoreOutcome::Missing)) as Arc<dyn BlobStore>,
            key(),
        )];

        assert_eq!(
            verify_replica_candidates(expected, &missing, deadline).await,
            ReplicaVerification::Missing
        );
    }

    #[tokio::test]
    async fn inv062_corrupt_replica_set_is_typed_corrupt() {
        let expected = ExpectedBlob::new(digest(1), NonZeroU64::MIN);
        let deadline = Instant::now() + Duration::from_secs(1);
        let corrupt = [(
            Arc::new(FakeStore(StoreOutcome::Corrupt)) as Arc<dyn BlobStore>,
            key(),
        )];

        assert_eq!(
            verify_replica_candidates(expected, &corrupt, deadline).await,
            ReplicaVerification::Corrupt
        );
    }

    #[tokio::test]
    async fn inv062_empty_replica_set_is_typed_missing() {
        let expected = ExpectedBlob::new(digest(1), NonZeroU64::MIN);
        let deadline = Instant::now() + Duration::from_secs(1);
        let empty: [(Arc<dyn BlobStore>, BlobObjectKey); 0] = [];

        assert_eq!(
            verify_replica_candidates(expected, &empty, deadline).await,
            ReplicaVerification::Missing
        );
    }

    #[tokio::test]
    async fn inv062_unavailable_replica_takes_precedence_over_corrupt() {
        let expected = ExpectedBlob::new(digest(1), NonZeroU64::MIN);
        let deadline = Instant::now() + Duration::from_secs(1);
        let mixed = [
            (
                Arc::new(FakeStore(StoreOutcome::Corrupt)) as Arc<dyn BlobStore>,
                key(),
            ),
            (
                Arc::new(FakeStore(StoreOutcome::Unavailable)) as Arc<dyn BlobStore>,
                key(),
            ),
        ];

        assert_eq!(
            verify_replica_candidates(expected, &mixed, deadline).await,
            ReplicaVerification::Unavailable
        );
    }

    #[tokio::test]
    async fn inv062_publication_ambiguity_is_typed_unavailable() {
        let expected = ExpectedBlob::new(digest(1), NonZeroU64::MIN);
        let deadline = Instant::now() + Duration::from_secs(1);
        let ambiguous = [(
            Arc::new(FakeStore(StoreOutcome::PublicationAmbiguous)) as Arc<dyn BlobStore>,
            key(),
        )];

        assert_eq!(
            verify_replica_candidates(expected, &ambiguous, deadline).await,
            ReplicaVerification::Unavailable
        );
    }

    #[test]
    fn distinct_attachment_lengths_are_checked_once_against_the_named_bound() {
        let attachments = [
            (
                digest(1),
                NonZeroU64::new(4).expect("fixture length is positive"),
            ),
            (
                digest(2),
                NonZeroU64::new(5).expect("fixture length is positive"),
            ),
        ];

        assert!(!attachment_lengths_exceed(attachments.iter().copied(), 9));
        assert!(attachment_lengths_exceed(attachments.iter().copied(), 8));
    }
}
