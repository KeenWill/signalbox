//! Bounded catalog-backed blob reads with no database transaction across store I/O.

use std::{io::Cursor, num::NonZeroU64, time::Duration};

use signalbox_blob_store::{BlobStoreFailureKind, MAX_BLOB_RANGE_BYTES};
use signalbox_domain::BlobDigest;
use signalbox_persistence::blob::{
    BlobCatalogEntry, BlobCatalogRepository, BlobCatalogRepositoryError,
};
use tokio::io::AsyncReadExt;

use crate::blob_storage_runtime::BlobStoreRegistry;

/// Hard safety ceiling bounding store latency and retained read capacity.
// numeric-bound: guard - prevents a stalled blob store read from blocking its caller forever
pub(crate) const BLOB_READ_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Bounded catalog facts returned without contacting a store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlobMetadata {
    pub(crate) byte_length: u64,
    pub(crate) replica_count: u64,
}

/// One direct read outcome with content-silent failure classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlobReadError {
    NotFound,
    RangeOutOfBounds { blob_length: u64 },
    Missing,
    Corrupt,
    Unavailable,
    Integrity,
}

pub(crate) async fn read_blob_metadata(
    repository: &BlobCatalogRepository,
    digest: BlobDigest,
) -> Result<BlobMetadata, BlobReadError> {
    let entry = read_blob_entry(repository, digest).await?;
    let replica_count =
        u64::try_from(entry.replicas().len()).map_err(|_| BlobReadError::Integrity)?;
    Ok(BlobMetadata {
        byte_length: entry.expected().byte_length(),
        replica_count,
    })
}

pub(crate) async fn read_blob_entry(
    repository: &BlobCatalogRepository,
    digest: BlobDigest,
) -> Result<BlobCatalogEntry, BlobReadError> {
    repository
        .find(digest)
        .await
        .map_err(map_catalog_error)?
        .ok_or(BlobReadError::NotFound)
}

pub(crate) async fn read_blob_chunk(
    registry: &BlobStoreRegistry,
    entry: &BlobCatalogEntry,
    offset: u64,
    length: NonZeroU64,
) -> Result<Vec<u8>, BlobReadError> {
    debug_assert!(length.get() <= MAX_BLOB_RANGE_BYTES);
    let expected = entry.expected();
    if offset
        .checked_add(length.get())
        .is_none_or(|end| end > expected.byte_length())
    {
        return Err(BlobReadError::RangeOutOfBounds {
            blob_length: expected.byte_length(),
        });
    }
    let mut saw_missing = false;
    let mut saw_corrupt = false;
    let mut saw_unavailable = false;
    for replica in entry.replicas() {
        let Some(store) = registry.recorded_store(replica.store()) else {
            return Err(BlobReadError::Integrity);
        };
        match store
            .open_range(expected, replica.object_key(), offset, length)
            .await
        {
            Ok(opened) => {
                if opened.byte_length() != length.get() {
                    return Err(BlobReadError::Integrity);
                }
                let capacity =
                    usize::try_from(length.get()).map_err(|_| BlobReadError::Integrity)?;
                let mut bytes = Vec::with_capacity(capacity);
                let mut reader = opened.into_reader();
                if (&mut reader)
                    .take(length.get())
                    .read_to_end(&mut bytes)
                    .await
                    .is_err()
                {
                    saw_unavailable = true;
                    continue;
                }
                if bytes.len() != capacity {
                    return Err(BlobReadError::Integrity);
                }
                let mut trailing = [0_u8; 1];
                match reader.read(&mut trailing).await {
                    Ok(0) => return Ok(bytes),
                    Ok(_) => return Err(BlobReadError::Integrity),
                    Err(_) => {
                        saw_unavailable = true;
                        continue;
                    }
                }
            }
            Err(error) => match error.kind() {
                BlobStoreFailureKind::NotFound => saw_missing = true,
                BlobStoreFailureKind::VerificationFailed => saw_corrupt = true,
                BlobStoreFailureKind::PublicationAmbiguous | BlobStoreFailureKind::Unavailable => {
                    saw_unavailable = true;
                }
            },
        }
    }
    if saw_unavailable {
        Err(BlobReadError::Unavailable)
    } else if saw_corrupt {
        Err(BlobReadError::Corrupt)
    } else if saw_missing {
        Err(BlobReadError::Missing)
    } else {
        Err(BlobReadError::Integrity)
    }
}

/// Opens one bounded HTTP range after the store verifies the complete object.
pub(crate) async fn open_recorded_blob_range(
    registry: &BlobStoreRegistry,
    entry: &BlobCatalogEntry,
    offset: u64,
    length: NonZeroU64,
) -> Result<signalbox_blob_store::BlobReader, BlobReadError> {
    let expected = entry.expected();
    if length.get() > MAX_BLOB_RANGE_BYTES
        || offset
            .checked_add(length.get())
            .is_none_or(|end| end > expected.byte_length())
    {
        return Err(BlobReadError::RangeOutOfBounds {
            blob_length: expected.byte_length(),
        });
    }
    let bytes = read_blob_chunk(registry, entry, offset, length).await?;
    Ok(Box::new(Cursor::new(bytes)))
}

/// Opens one generation-pinned stream after a single complete-object verification pass.
pub(crate) async fn open_recorded_blob_verified(
    registry: &BlobStoreRegistry,
    entry: &BlobCatalogEntry,
) -> Result<signalbox_blob_store::BlobReader, BlobReadError> {
    let mut saw_missing = false;
    let mut saw_corrupt = false;
    let mut saw_unavailable = false;
    for replica in entry.replicas() {
        let Some(store) = registry.recorded_store(replica.store()) else {
            return Err(BlobReadError::Integrity);
        };
        match store
            .open_verified(entry.expected(), replica.object_key())
            .await
        {
            Ok(opened) if opened.byte_length() == entry.expected().byte_length() => {
                return Ok(opened.into_reader());
            }
            Ok(_) => saw_corrupt = true,
            Err(error) => match error.kind() {
                BlobStoreFailureKind::NotFound => saw_missing = true,
                BlobStoreFailureKind::VerificationFailed => saw_corrupt = true,
                BlobStoreFailureKind::PublicationAmbiguous | BlobStoreFailureKind::Unavailable => {
                    saw_unavailable = true;
                }
            },
        }
    }
    if saw_unavailable {
        Err(BlobReadError::Unavailable)
    } else if saw_corrupt {
        Err(BlobReadError::Corrupt)
    } else if saw_missing {
        Err(BlobReadError::Missing)
    } else {
        Err(BlobReadError::Integrity)
    }
}

fn map_catalog_error(error: BlobCatalogRepositoryError) -> BlobReadError {
    match error {
        BlobCatalogRepositoryError::Database(_)
        | BlobCatalogRepositoryError::CommitAmbiguous(_) => BlobReadError::Unavailable,
        BlobCatalogRepositoryError::Corruption(_) => BlobReadError::Integrity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// direct reads share the wire and store range bound.
    fn direct_read_bound_matches_the_wire_and_store_contract() {
        assert_eq!(
            MAX_BLOB_RANGE_BYTES,
            signalbox_process_protocol::MAX_BLOB_READ_BYTES as u64
        );
    }
}
