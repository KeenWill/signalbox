//! Bounded catalog-backed blob reads with no database transaction across store I/O.

use std::{io::SeekFrom, num::NonZeroU64};

use sha2::{Digest as _, Sha256};
use signalbox_blob_store::{BlobStoreFailureKind, MAX_BLOB_RANGE_BYTES};
use signalbox_domain::BlobDigest;
use signalbox_persistence::blob::{
    BlobCatalogEntry, BlobCatalogRepository, BlobCatalogRepositoryError,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::blob_storage_runtime::BlobStoreRegistry;

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
                BlobStoreFailureKind::Unavailable => saw_unavailable = true,
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

/// Opens one published replica, verifies all bytes, and advances to an HTTP range.
///
/// Verification spools to an anonymous file so no bytes are exposed under an
/// immutable digest until the complete candidate matches the catalog identity.
pub(crate) async fn open_recorded_blob_range(
    registry: &BlobStoreRegistry,
    entry: &BlobCatalogEntry,
    offset: u64,
    length: NonZeroU64,
) -> Result<signalbox_blob_store::BlobReader, BlobReadError> {
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
        match store.open(replica.object_key()).await {
            Ok(opened) if opened.byte_length() == expected.byte_length() => {
                let mut reader = opened.into_reader();
                let temporary = tempfile::tempfile().map_err(|_| BlobReadError::Unavailable)?;
                let mut verified = tokio::fs::File::from_std(temporary);
                let mut hasher = Sha256::new();
                let mut observed = 0_u64;
                let mut buffer = [0_u8; 64 * 1024];
                let mut unavailable = false;
                loop {
                    let read = match reader.read(&mut buffer).await {
                        Ok(read) => read,
                        Err(_) => {
                            unavailable = true;
                            break;
                        }
                    };
                    if read == 0 {
                        break;
                    }
                    observed = observed
                        .checked_add(u64::try_from(read).map_err(|_| BlobReadError::Integrity)?)
                        .ok_or(BlobReadError::Integrity)?;
                    if observed > expected.byte_length() {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                    verified
                        .write_all(&buffer[..read])
                        .await
                        .map_err(|_| BlobReadError::Unavailable)?;
                }
                if unavailable {
                    saw_unavailable = true;
                    continue;
                }
                let observed_digest = BlobDigest::from_bytes(hasher.finalize().into());
                if observed != expected.byte_length() || observed_digest != expected.digest() {
                    saw_corrupt = true;
                    continue;
                }
                verified
                    .flush()
                    .await
                    .map_err(|_| BlobReadError::Unavailable)?;
                verified
                    .seek(SeekFrom::Start(offset))
                    .await
                    .map_err(|_| BlobReadError::Unavailable)?;
                return Ok(Box::new(verified.take(length.get())));
            }
            Ok(_) => saw_corrupt = true,
            Err(error) => match error.kind() {
                BlobStoreFailureKind::NotFound => saw_missing = true,
                BlobStoreFailureKind::VerificationFailed => saw_corrupt = true,
                BlobStoreFailureKind::Unavailable => saw_unavailable = true,
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
    /// INV-060: direct reads share the wire and store range bound.
    fn direct_read_bound_matches_the_wire_and_store_contract() {
        assert_eq!(
            MAX_BLOB_RANGE_BYTES,
            signalbox_process_protocol::MAX_BLOB_READ_BYTES as u64
        );
    }
}
