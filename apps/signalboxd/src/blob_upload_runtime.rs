//! Connection-local streaming blob upload orchestration.

use std::{fmt, sync::Arc};

use sha2::{Digest as _, Sha256};
use signalbox_blob_store::{
    BlobPutOutcome, BlobStore, BlobStoreFailureKind, BlobStoreName, ExpectedBlob,
};
use signalbox_blob_store_filesystem::FilesystemBlobUpload;
use signalbox_domain::BlobDigest;
use signalbox_persistence::blob::{
    BlobCatalogRepository, BlobCatalogRepositoryError, BlobReplicaRecord, BlobStoreBindingRecord,
};
use tokio::io::AsyncReadExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;
use uuid::Uuid;

use crate::{BlobStorageClass, BlobStoreRegistry};

const VERIFICATION_BUFFER_BYTES: usize = 64 * 1024;

/// One active upload whose bytes remain solely in a private disk spool.
pub(crate) struct PendingBlobUpload {
    expected: ExpectedBlob,
    store_name: BlobStoreName,
    namespace_id: Uuid,
    store: Arc<dyn BlobStore>,
    observed_length: u64,
    digest: Sha256,
    spool: FilesystemBlobUpload,
    _bulk_permit: OwnedSemaphorePermit,
    started_at: Instant,
    idle_since: Instant,
}

impl fmt::Debug for PendingBlobUpload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingBlobUpload")
            .field("expected", &self.expected)
            .field("store_name", &self.store_name)
            .field("namespace_id", &self.namespace_id)
            .field("observed_length", &self.observed_length)
            .field("spool", &self.spool)
            .finish_non_exhaustive()
    }
}

/// Result of beginning an upload against the current semantic route.
pub(crate) enum BeginBlobUploadOutcome {
    Begun(Box<PendingBlobUpload>),
    AlreadyPresent(ExpectedBlob),
}

/// Blob upload failure before, during, or after store publication.
#[derive(Debug)]
pub(crate) enum BlobUploadError {
    SizeExceeded { observed: u64 },
    LengthMismatch { observed: u64 },
    DigestMismatch { observed: BlobDigest },
    Unavailable,
    PublicationAmbiguous,
    CommitAmbiguous,
    Integrity,
}

pub(crate) async fn begin_blob_upload(
    registry: &BlobStoreRegistry,
    repository: &BlobCatalogRepository,
    expected: ExpectedBlob,
    bulk_permit: OwnedSemaphorePermit,
    started_at: Instant,
) -> Result<BeginBlobUploadOutcome, BlobUploadError> {
    let (store_name, store) = registry.routed_store(BlobStorageClass::UserAttachment);
    if let Some(entry) = repository
        .find(expected.digest())
        .await
        .map_err(map_catalog_error)?
    {
        if entry.expected().byte_length() != expected.byte_length() {
            return Err(BlobUploadError::Integrity);
        }
        if let Some(replica) = entry.replica_in_store(store_name) {
            match verify_replica(store.as_ref(), expected, replica.object_key()).await {
                Ok(()) => return Ok(BeginBlobUploadOutcome::AlreadyPresent(expected)),
                Err(BlobUploadError::Integrity) => {}
                Err(error) => return Err(error),
            }
        }
    }
    let spool = registry
        .staging()
        .create_upload()
        .await
        .map_err(|_| BlobUploadError::Unavailable)?;
    Ok(BeginBlobUploadOutcome::Begun(Box::new(PendingBlobUpload {
        expected,
        store_name: store_name.clone(),
        namespace_id: registry.namespace_id(store_name),
        store,
        observed_length: 0,
        digest: Sha256::new(),
        spool,
        _bulk_permit: bulk_permit,
        started_at,
        idle_since: started_at,
    })))
}

impl PendingBlobUpload {
    pub(crate) const fn expected(&self) -> ExpectedBlob {
        self.expected
    }

    pub(crate) const fn started_at(&self) -> Instant {
        self.started_at
    }

    pub(crate) const fn idle_since(&self) -> Instant {
        self.idle_since
    }

    pub(crate) fn mark_activity_complete(&mut self) {
        self.idle_since = Instant::now();
    }

    pub(crate) async fn append(&mut self, chunk: &[u8]) -> Result<u64, BlobUploadError> {
        let chunk_length = u64::try_from(chunk.len())
            .map_err(|_| BlobUploadError::SizeExceeded { observed: u64::MAX })?;
        let observed = self
            .observed_length
            .checked_add(chunk_length)
            .ok_or(BlobUploadError::SizeExceeded { observed: u64::MAX })?;
        if observed > self.expected.byte_length() {
            return Err(BlobUploadError::SizeExceeded { observed });
        }
        self.spool
            .append(chunk)
            .await
            .map_err(|_| BlobUploadError::Unavailable)?;
        self.digest.update(chunk);
        self.observed_length = observed;
        Ok(observed)
    }

    pub(crate) async fn commit(
        self,
        repository: &BlobCatalogRepository,
    ) -> Result<ExpectedBlob, BlobUploadError> {
        if self.observed_length != self.expected.byte_length() {
            return Err(BlobUploadError::LengthMismatch {
                observed: self.observed_length,
            });
        }
        let observed_digest = BlobDigest::from_bytes(self.digest.finalize().into());
        if observed_digest != self.expected.digest() {
            return Err(BlobUploadError::DigestMismatch {
                observed: observed_digest,
            });
        }
        let reader = self
            .spool
            .into_reader()
            .await
            .map_err(|_| BlobUploadError::Unavailable)?;
        let publication = self
            .store
            .put(self.expected, reader)
            .await
            .map_err(map_store_error)?;
        register_publication(
            repository,
            self.expected,
            self.store_name,
            self.namespace_id,
            publication,
        )
        .await?;
        Ok(self.expected)
    }
}

async fn register_publication(
    repository: &BlobCatalogRepository,
    expected: ExpectedBlob,
    store_name: BlobStoreName,
    namespace_id: Uuid,
    publication: BlobPutOutcome,
) -> Result<(), BlobUploadError> {
    let key = publication.key().clone();
    repository
        .register_verified_replica(
            expected,
            BlobStoreBindingRecord::new(store_name.clone(), namespace_id),
            BlobReplicaRecord::new(store_name, key),
        )
        .await
        .map_err(map_catalog_error)?;
    Ok(())
}

async fn verify_replica(
    store: &dyn BlobStore,
    expected: ExpectedBlob,
    key: &signalbox_blob_store::BlobObjectKey,
) -> Result<(), BlobUploadError> {
    let opened = store.open(key).await.map_err(map_store_error)?;
    if opened.byte_length() != expected.byte_length() {
        return Err(BlobUploadError::Integrity);
    }
    let mut reader = opened.into_reader();
    let mut digest = Sha256::new();
    let mut observed = 0_u64;
    let mut buffer = vec![0_u8; VERIFICATION_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|_| BlobUploadError::Unavailable)?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(u64::try_from(read).map_err(|_| BlobUploadError::Integrity)?)
            .ok_or(BlobUploadError::Integrity)?;
        if observed > expected.byte_length() {
            return Err(BlobUploadError::Integrity);
        }
        digest.update(&buffer[..read]);
    }
    let observed_digest = BlobDigest::from_bytes(digest.finalize().into());
    if observed == expected.byte_length() && observed_digest == expected.digest() {
        Ok(())
    } else {
        Err(BlobUploadError::Integrity)
    }
}

fn map_store_error(error: signalbox_blob_store::BlobStoreError) -> BlobUploadError {
    match error.kind() {
        BlobStoreFailureKind::NotFound | BlobStoreFailureKind::VerificationFailed => {
            BlobUploadError::Integrity
        }
        BlobStoreFailureKind::PublicationAmbiguous => BlobUploadError::PublicationAmbiguous,
        BlobStoreFailureKind::Unavailable => BlobUploadError::Unavailable,
    }
}

fn map_catalog_error(error: BlobCatalogRepositoryError) -> BlobUploadError {
    match error {
        BlobCatalogRepositoryError::Database(_) => BlobUploadError::Unavailable,
        BlobCatalogRepositoryError::CommitAmbiguous(_) => BlobUploadError::CommitAmbiguous,
        BlobCatalogRepositoryError::Corruption(_) => BlobUploadError::Integrity,
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "focused unit fixtures use assertion panics and explicit setup expectations"
)]
mod tests {
    use super::*;
    use signalbox_blob_store::{
        BlobObjectKey, BlobReader, BlobStoreError, BlobStoreFuture, OpenedBlob,
    };
    use signalbox_blob_store_filesystem::FilesystemBlobStaging;
    use sqlx::postgres::PgPoolOptions;
    use std::{fs, os::unix::fs::PermissionsExt as _};

    #[derive(Debug)]
    struct UncalledStore;

    impl BlobStore for UncalledStore {
        fn put<'a>(
            &'a self,
            _expected: ExpectedBlob,
            _source: BlobReader,
        ) -> BlobStoreFuture<'a, BlobPutOutcome> {
            Box::pin(async { Err(BlobStoreError::unavailable("unexpected test publication")) })
        }

        fn open<'a>(&'a self, _key: &'a BlobObjectKey) -> BlobStoreFuture<'a, OpenedBlob> {
            Box::pin(async { Err(BlobStoreError::unavailable("unexpected test open")) })
        }

        fn open_verified<'a>(
            &'a self,
            _expected: ExpectedBlob,
            _key: &'a BlobObjectKey,
        ) -> BlobStoreFuture<'a, OpenedBlob> {
            Box::pin(async { Err(BlobStoreError::unavailable("unexpected verified test open")) })
        }

        fn open_range<'a>(
            &'a self,
            _expected: ExpectedBlob,
            _key: &'a BlobObjectKey,
            _offset: u64,
            _byte_length: std::num::NonZeroU64,
        ) -> BlobStoreFuture<'a, OpenedBlob> {
            Box::pin(async { Err(BlobStoreError::unavailable("unexpected test range")) })
        }
    }

    async fn pending_fixture(expected: ExpectedBlob) -> (tempfile::TempDir, PendingBlobUpload) {
        let root = tempfile::TempDir::new().expect("the fixture creates a staging root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
            .expect("the fixture staging root is private");
        let staging = FilesystemBlobStaging::try_new(root.path().to_path_buf())
            .expect("the fixture staging namespace opens");
        let spool = staging
            .create_upload()
            .await
            .expect("the fixture creates one upload spool");
        let upload = PendingBlobUpload {
            expected,
            store_name: BlobStoreName::try_new("primary").expect("fixture store name is valid"),
            namespace_id: Uuid::from_u128(1),
            store: Arc::new(UncalledStore),
            observed_length: 0,
            digest: Sha256::new(),
            spool,
            _bulk_permit: Arc::new(tokio::sync::Semaphore::new(1))
                .acquire_owned()
                .await
                .expect("fixture bulk-ingest permit is open"),
            started_at: Instant::now(),
            idle_since: Instant::now(),
        };
        (root, upload)
    }

    /// exceeding either the declared length or deployment ceiling
    /// rejects before the bytes enter the private spool.
    #[tokio::test]
    async fn append_rejects_an_oversized_cumulative_length() {
        let expected =
            ExpectedBlob::try_new(BlobDigest::digest(b"abc"), 3).expect("fixture blob is nonempty");
        let (_root, mut upload) = pending_fixture(expected).await;

        let error = upload
            .append(b"abcd")
            .await
            .expect_err("a fourth byte exceeds the declared length");

        let BlobUploadError::SizeExceeded { observed } = error else {
            panic!("oversized append returned another error class")
        };
        assert_eq!(observed, 4);
    }

    /// commit rejects a short assembled stream before publication or
    /// catalog access.
    #[tokio::test]
    async fn commit_rejects_a_short_stream_before_publication() {
        let expected =
            ExpectedBlob::try_new(BlobDigest::digest(b"abc"), 3).expect("fixture blob is nonempty");
        let (_root, mut upload) = pending_fixture(expected).await;
        upload
            .append(b"ab")
            .await
            .expect("the bounded partial chunk appends");
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://fixture:fixture@127.0.0.1/fixture")
            .expect("the fixture pool parses without connecting");
        let repository = BlobCatalogRepository::new(pool);

        let error = upload
            .commit(&repository)
            .await
            .expect_err("a short stream must not reach publication");

        let BlobUploadError::LengthMismatch { observed } = error else {
            panic!("short commit returned another error class")
        };
        assert_eq!(observed, 2);
    }

    /// equal length with different bytes rejects before publication
    /// or catalog access and retains only the observed digest as evidence.
    #[tokio::test]
    async fn commit_rejects_a_digest_mismatch_before_publication() {
        let expected =
            ExpectedBlob::try_new(BlobDigest::digest(b"abc"), 3).expect("fixture blob is nonempty");
        let (_root, mut upload) = pending_fixture(expected).await;
        upload
            .append(b"abd")
            .await
            .expect("the equal-length chunk appends");
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://fixture:fixture@127.0.0.1/fixture")
            .expect("the fixture pool parses without connecting");
        let repository = BlobCatalogRepository::new(pool);

        let error = upload
            .commit(&repository)
            .await
            .expect_err("different bytes must not reach publication");

        let BlobUploadError::DigestMismatch { observed } = error else {
            panic!("digest mismatch returned another error class")
        };
        assert_eq!(observed, BlobDigest::digest(b"abd"));
    }

    /// an adapter that cannot reconcile a possible publication keeps
    /// that ambiguity distinct for the wire retry contract.
    #[test]
    fn store_publication_ambiguity_survives_upload_mapping() {
        let mapped = map_store_error(BlobStoreError::publication_ambiguous(
            "reconcile fixture publication",
        ));

        assert!(matches!(mapped, BlobUploadError::PublicationAmbiguous));
    }
}

/// Publishes and verifies one bounded derived view, then registers its immutable replica.
pub(crate) async fn publish_generated_artifact(
    registry: &BlobStoreRegistry,
    repository: &BlobCatalogRepository,
    artifact: &signalbox_file_media_runtime::ValidatedMediaArtifact,
) -> Result<(), signalbox_tools_file_media::FileMediaServiceFailure> {
    let reference = artifact.reference();
    let expected = ExpectedBlob::new(
        BlobDigest::from_bytes(*reference.presented().digest().as_bytes()),
        reference.byte_length(),
    );
    if expected.byte_length() > registry.max_blob_bytes() {
        return Err(signalbox_file_media_runtime::FileMediaFailure::OutputUnitTooLarge.into());
    }
    let (store_name, store) = registry.routed_store(BlobStorageClass::GeneratedArtifact);
    publish_generated_bytes(
        registry.staging(),
        store.as_ref(),
        repository,
        BlobStoreBindingRecord::new(store_name.clone(), registry.namespace_id(store_name)),
        expected,
        artifact.bytes(),
    )
    .await
}

async fn publish_generated_bytes(
    staging: &signalbox_blob_store_filesystem::FilesystemBlobStaging,
    store: &dyn BlobStore,
    repository: &BlobCatalogRepository,
    binding: BlobStoreBindingRecord,
    expected: ExpectedBlob,
    bytes: &[u8],
) -> Result<(), signalbox_tools_file_media::FileMediaServiceFailure> {
    use signalbox_application::OperatorFailureClass;
    use signalbox_tools_file_media::{FileMediaExecutorError, FileMediaServiceFailure};
    let operator =
        |class| FileMediaServiceFailure::Operator(FileMediaExecutorError::from_class(class));
    let spool = staging.create_upload().await.map_err(|_| {
        operator(OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        })
    })?;
    let mut spool = spool;
    spool.append(bytes).await.map_err(|_| {
        operator(OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        })
    })?;
    let reader = spool.into_reader().await.map_err(|_| {
        operator(OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        })
    })?;
    let publication = store
        .put(expected, reader)
        .await
        .map_err(|error| match error.kind() {
            BlobStoreFailureKind::VerificationFailed => {
                operator(OperatorFailureClass::FailClosedCorruption)
            }
            _ => operator(OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }),
        })?;
    verify_replica(store, expected, publication.key())
        .await
        .map_err(|error| match error {
            BlobUploadError::Integrity => operator(OperatorFailureClass::FailClosedCorruption),
            _ => operator(OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }),
        })?;
    repository
        .register_verified_replica(
            expected,
            binding.clone(),
            BlobReplicaRecord::new(binding.store().clone(), publication.key().clone()),
        )
        .await
        .map_err(|error| match error {
            BlobCatalogRepositoryError::Database(_) => {
                operator(OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                })
            }
            BlobCatalogRepositoryError::CommitAmbiguous(_) => {
                operator(OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                })
            }
            BlobCatalogRepositoryError::Corruption(_) => {
                operator(OperatorFailureClass::FailClosedCorruption)
            }
        })?;
    Ok(())
}

#[cfg(all(test, unix))]
mod generated_tests {
    use super::*;
    use signalbox_application::{ClassifyOperatorFailure as _, OperatorFailureClass};
    use signalbox_blob_store::{
        BlobObjectKey, BlobReader, BlobStoreError, BlobStoreFuture, OpenedBlob,
    };
    use std::{
        num::NonZeroU64,
        os::unix::fs::PermissionsExt as _,
        sync::atomic::{AtomicUsize, Ordering::Relaxed},
    };

    #[derive(Debug)]
    struct Store {
        publication_fails: bool,
        corrupt: bool,
        calls: AtomicUsize,
    }
    impl BlobStore for Store {
        fn put<'a>(
            &'a self,
            expected: ExpectedBlob,
            mut source: BlobReader,
        ) -> BlobStoreFuture<'a, BlobPutOutcome> {
            Box::pin(async move {
                assert_eq!(self.calls.fetch_add(1, Relaxed), 0);
                let mut bytes = Vec::new();
                source
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|error| BlobStoreError::io("fixture read", error))?;
                assert_eq!(BlobDigest::digest(&bytes), expected.digest());
                if self.publication_fails {
                    return Err(BlobStoreError::unavailable("injected publication failure"));
                }
                Ok(BlobPutOutcome::Published {
                    key: BlobObjectKey::for_digest(expected.digest()),
                })
            })
        }
        fn open<'a>(&'a self, _: &'a BlobObjectKey) -> BlobStoreFuture<'a, OpenedBlob> {
            Box::pin(async move {
                assert_eq!(self.calls.fetch_add(1, Relaxed), 1);
                let bytes = if self.corrupt { b"bad" } else { b"art" };
                Ok(OpenedBlob::new(
                    3,
                    Box::new(std::io::Cursor::new(bytes.to_vec())),
                ))
            })
        }
        fn open_verified<'a>(
            &'a self,
            _: ExpectedBlob,
            _: &'a BlobObjectKey,
        ) -> BlobStoreFuture<'a, OpenedBlob> {
            Box::pin(async {
                Err(BlobStoreError::unavailable(
                    "unexpected fixture verified open",
                ))
            })
        }
        fn open_range<'a>(
            &'a self,
            _: ExpectedBlob,
            _: &'a BlobObjectKey,
            _: u64,
            _: NonZeroU64,
        ) -> BlobStoreFuture<'a, OpenedBlob> {
            Box::pin(async { Err(BlobStoreError::unavailable("unexpected fixture range")) })
        }
    }
    #[tokio::test]
    async fn file_generated_publication_verification_and_registration_fail_without_a_reference() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://invalid/fixture")
            .unwrap();
        pool.close().await;
        let repository = BlobCatalogRepository::new(pool);
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let staging = signalbox_blob_store_filesystem::FilesystemBlobStaging::try_new(
            root.path().to_path_buf(),
        )
        .unwrap();
        let expected = ExpectedBlob::new(BlobDigest::digest(b"art"), NonZeroU64::new(3).unwrap());
        for (publication_fails, corrupt, calls, class) in [
            (
                true,
                false,
                1,
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
            ),
            (false, true, 2, OperatorFailureClass::FailClosedCorruption),
            (
                false,
                false,
                2,
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
            ),
        ] {
            let store = Store {
                publication_fails,
                corrupt,
                calls: AtomicUsize::new(0),
            };
            let binding = BlobStoreBindingRecord::new(
                BlobStoreName::try_new("fixture").unwrap(),
                Uuid::from_u128(1),
            );
            let error =
                publish_generated_bytes(&staging, &store, &repository, binding, expected, b"art")
                    .await
                    .unwrap_err();
            let signalbox_tools_file_media::FileMediaServiceFailure::Operator(error) = error else {
                panic!("operator failure must retain its class")
            };
            assert_eq!(error.operator_failure_class(), class);
            assert_eq!(store.calls.load(Relaxed), calls);
        }
    }
}
