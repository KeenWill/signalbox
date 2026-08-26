//! Bounded blob publication and checked reads for imported raw source records.

use std::{fmt, io::Cursor, sync::Arc, time::Duration};

use sha2::{Digest as _, Sha256};
use signalbox_blob_store::{
    BlobPutOutcome, BlobReader, BlobStore, BlobStoreFailureKind, ExpectedBlob,
};
use signalbox_domain::BlobDigest;
use signalbox_persistence::{
    blob::{BlobCatalogRepository, BlobCatalogRepositoryError},
    conversation_import::{
        ImportedRawBlobInput, ImportedRawBlobPublication, ImportedRawBlobPublicationFuture,
        ImportedRawBlobReadFuture, ImportedRawBlobStorage, ImportedRawBlobStorageError,
    },
};
use sqlx::PgPool;
use tokio::{io::AsyncReadExt, sync::Semaphore, time::timeout};

use crate::{BlobStorageClass, BlobStoreRegistry};

const VERIFICATION_BUFFER_BYTES: usize = 64 * 1024;
// numeric-bound: guard - prevents a stalled imported-blob read traversal from blocking its caller forever
const READ_TRAVERSAL_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Deployment adapter joining imported aggregates to immutable blob stores.
#[derive(Clone)]
pub(crate) struct ImportedSourceBlobStorage {
    registry: Option<Arc<BlobStoreRegistry>>,
    catalog: BlobCatalogRepository,
    read_budget: Arc<Semaphore>,
    maximum_source_bytes: u64,
}

impl ImportedSourceBlobStorage {
    pub(crate) fn new(
        pool: PgPool,
        registry: Option<Arc<BlobStoreRegistry>>,
        maximum_source_bytes: usize,
    ) -> Self {
        let read_budget = registry.as_ref().map_or_else(
            || Arc::new(Semaphore::new(0)),
            |registry| registry.read_budget(),
        );
        Self {
            registry,
            catalog: BlobCatalogRepository::new(pool),
            read_budget,
            maximum_source_bytes: u64::try_from(maximum_source_bytes).unwrap_or(u64::MAX),
        }
    }
}

impl fmt::Debug for ImportedSourceBlobStorage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedSourceBlobStorage")
            .field("configured", &self.registry.is_some())
            .field("maximum_source_bytes", &self.maximum_source_bytes)
            .finish_non_exhaustive()
    }
}

impl ImportedRawBlobStorage for ImportedSourceBlobStorage {
    fn publish(&self, blobs: Box<[ImportedRawBlobInput]>) -> ImportedRawBlobPublicationFuture<'_> {
        Box::pin(async move {
            let registry = self
                .registry
                .as_deref()
                .ok_or(ImportedRawBlobStorageError::Unavailable)?;
            enforce_cumulative_bound(
                blobs.iter().map(|blob| blob.expected()),
                self.maximum_source_bytes,
            )?;
            let mut publications = Vec::with_capacity(blobs.len());
            for blob in &blobs {
                publications.push(self.publish_one(registry, blob).await?);
            }
            Ok(publications.into_boxed_slice())
        })
    }

    fn read(
        &self,
        blobs: Box<[ExpectedBlob]>,
        total_source_bytes: u64,
    ) -> ImportedRawBlobReadFuture<'_> {
        Box::pin(async move {
            let registry = self
                .registry
                .as_deref()
                .ok_or(ImportedRawBlobStorageError::Unavailable)?;
            if total_source_bytes > self.maximum_source_bytes {
                return Err(ImportedRawBlobStorageError::Integrity);
            }
            enforce_cumulative_bound(blobs.iter().copied(), self.maximum_source_bytes)?;
            let permit = Arc::clone(&self.read_budget)
                .try_acquire_owned()
                .map_err(|_| ImportedRawBlobStorageError::Unavailable)?;
            let contents = timeout(READ_TRAVERSAL_TIMEOUT, async {
                let mut contents = Vec::with_capacity(blobs.len());
                for expected in &blobs {
                    contents.push(self.read_one(registry, *expected).await?);
                }
                Ok::<_, ImportedRawBlobStorageError>(contents)
            })
            .await
            .map_err(|_| ImportedRawBlobStorageError::Unavailable)??;
            drop(permit);
            Ok(contents.into_boxed_slice())
        })
    }
}

impl ImportedSourceBlobStorage {
    async fn publish_one(
        &self,
        registry: &BlobStoreRegistry,
        blob: &ImportedRawBlobInput,
    ) -> Result<ImportedRawBlobPublication, ImportedRawBlobStorageError> {
        let expected = blob.expected();
        let (store_name, store) = registry.routed_store(BlobStorageClass::ImportedSource);
        if let Some(entry) = self
            .catalog
            .find(expected.digest())
            .await
            .map_err(map_catalog_error)?
        {
            if entry.expected().byte_length() != expected.byte_length() {
                return Err(ImportedRawBlobStorageError::Integrity);
            }
            if let Some(replica) = entry.replica_in_store(store_name) {
                match verify_store_object(store.as_ref(), expected, replica.object_key()).await {
                    Ok(()) => {
                        return Ok(ImportedRawBlobPublication::new(
                            expected,
                            store_name.clone(),
                            registry.namespace_id(store_name),
                            replica.object_key().clone(),
                        ));
                    }
                    Err(ImportedRawBlobStorageError::Integrity) => {}
                    Err(ImportedRawBlobStorageError::Unavailable) => {
                        return Err(ImportedRawBlobStorageError::Unavailable);
                    }
                }
            }
        }
        let source: BlobReader = Box::new(Cursor::new(blob.shared_bytes()));
        let publication = store.put(expected, source).await.map_err(map_store_error)?;
        Ok(publication_facts(
            registry,
            store_name,
            expected,
            publication,
        ))
    }

    async fn read_one(
        &self,
        registry: &BlobStoreRegistry,
        expected: ExpectedBlob,
    ) -> Result<Vec<u8>, ImportedRawBlobStorageError> {
        let entry = self
            .catalog
            .find(expected.digest())
            .await
            .map_err(map_catalog_error)?
            .ok_or(ImportedRawBlobStorageError::Integrity)?;
        if entry.expected() != expected {
            return Err(ImportedRawBlobStorageError::Integrity);
        }
        let mut unavailable = false;
        for replica in entry.replicas() {
            let store = registry
                .recorded_store(replica.store())
                .ok_or(ImportedRawBlobStorageError::Integrity)?;
            match read_and_verify(store.as_ref(), expected, replica.object_key()).await {
                Ok(bytes) => return Ok(bytes),
                Err(ImportedRawBlobStorageError::Unavailable) => unavailable = true,
                Err(ImportedRawBlobStorageError::Integrity) => {}
            }
        }
        if unavailable {
            Err(ImportedRawBlobStorageError::Unavailable)
        } else {
            Err(ImportedRawBlobStorageError::Integrity)
        }
    }
}

fn enforce_cumulative_bound(
    blobs: impl IntoIterator<Item = ExpectedBlob>,
    maximum_source_bytes: u64,
) -> Result<(), ImportedRawBlobStorageError> {
    let mut total = 0_u64;
    for blob in blobs {
        total = total
            .checked_add(blob.byte_length())
            .ok_or(ImportedRawBlobStorageError::Integrity)?;
        if total > maximum_source_bytes {
            return Err(ImportedRawBlobStorageError::Integrity);
        }
    }
    Ok(())
}

fn publication_facts(
    registry: &BlobStoreRegistry,
    store_name: &signalbox_blob_store::BlobStoreName,
    expected: ExpectedBlob,
    publication: BlobPutOutcome,
) -> ImportedRawBlobPublication {
    ImportedRawBlobPublication::new(
        expected,
        store_name.clone(),
        registry.namespace_id(store_name),
        publication.key().clone(),
    )
}

async fn verify_store_object(
    store: &dyn BlobStore,
    expected: ExpectedBlob,
    key: &signalbox_blob_store::BlobObjectKey,
) -> Result<(), ImportedRawBlobStorageError> {
    read_and_verify(store, expected, key).await.map(drop)
}

async fn read_and_verify(
    store: &dyn BlobStore,
    expected: ExpectedBlob,
    key: &signalbox_blob_store::BlobObjectKey,
) -> Result<Vec<u8>, ImportedRawBlobStorageError> {
    let opened = store.open(key).await.map_err(map_store_error)?;
    if opened.byte_length() != expected.byte_length() {
        return Err(ImportedRawBlobStorageError::Integrity);
    }
    let capacity = usize::try_from(expected.byte_length())
        .map_err(|_| ImportedRawBlobStorageError::Integrity)?;
    let mut reader = opened.into_reader();
    let mut bytes = allocate_blob_buffer(capacity)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; VERIFICATION_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|_| ImportedRawBlobStorageError::Unavailable)?;
        if read == 0 {
            break;
        }
        if bytes
            .len()
            .checked_add(read)
            .is_none_or(|observed| observed > capacity)
        {
            return Err(ImportedRawBlobStorageError::Integrity);
        }
        digest.update(&buffer[..read]);
        bytes.extend_from_slice(&buffer[..read]);
    }
    let observed = BlobDigest::from_bytes(digest.finalize().into());
    if bytes.len() != capacity || observed != expected.digest() {
        return Err(ImportedRawBlobStorageError::Integrity);
    }
    Ok(bytes)
}

fn allocate_blob_buffer(capacity: usize) -> Result<Vec<u8>, ImportedRawBlobStorageError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| ImportedRawBlobStorageError::Unavailable)?;
    Ok(bytes)
}

fn map_store_error(error: signalbox_blob_store::BlobStoreError) -> ImportedRawBlobStorageError {
    match error.kind() {
        BlobStoreFailureKind::PublicationAmbiguous | BlobStoreFailureKind::Unavailable => {
            ImportedRawBlobStorageError::Unavailable
        }
        BlobStoreFailureKind::NotFound | BlobStoreFailureKind::VerificationFailed => {
            ImportedRawBlobStorageError::Integrity
        }
    }
}

fn map_catalog_error(error: BlobCatalogRepositoryError) -> ImportedRawBlobStorageError {
    match error {
        BlobCatalogRepositoryError::Database(_)
        | BlobCatalogRepositoryError::CommitAmbiguous(_) => {
            ImportedRawBlobStorageError::Unavailable
        }
        BlobCatalogRepositoryError::Corruption(_) => ImportedRawBlobStorageError::Integrity,
    }
}

#[cfg(test)]
mod tests {
    use signalbox_blob_store::ExpectedBlob;
    use signalbox_domain::BlobDigest;

    use super::{ImportedRawBlobStorageError, allocate_blob_buffer, enforce_cumulative_bound};

    fn expected(byte: u8, length: u64) -> ExpectedBlob {
        ExpectedBlob::try_new(BlobDigest::from_bytes([byte; 32]), length)
            .expect("the fixture length is positive")
    }

    #[test]
    fn cumulative_import_bound_accepts_its_exact_limit() {
        let blobs = [expected(1, 2), expected(2, 3)];

        assert_eq!(enforce_cumulative_bound(blobs, 5), Ok(()));
    }

    #[test]
    fn cumulative_import_bound_rejects_one_excess_byte() {
        let blobs = [expected(1, 2), expected(2, 3)];

        assert_eq!(
            enforce_cumulative_bound(blobs, 4),
            Err(ImportedRawBlobStorageError::Integrity),
        );
    }

    #[test]
    fn imported_blob_buffer_reports_reservation_failure() {
        assert_eq!(
            allocate_blob_buffer(usize::MAX),
            Err(ImportedRawBlobStorageError::Unavailable),
        );
    }
}
