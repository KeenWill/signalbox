//! Same-filesystem atomic immutable-blob publication.
//!
//! The normative specification is `docs/spec/blob-storage.md`.

use std::{io, path::PathBuf};

use sha2::{Digest, Sha256};
use signalbox_blob_store::{
    BlobObjectKey, BlobPutOutcome, BlobReader, BlobStore, BlobStoreError, BlobStoreFuture,
    BlobVerificationFailure, ExpectedBlob, OpenedBlob,
};
use signalbox_domain::BlobDigest;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const STREAM_BUFFER_BYTES: usize = 64 * 1024;

/// Filesystem store rooted at one deployment-owned storage namespace.
#[derive(Clone, Debug)]
pub struct FilesystemBlobStore {
    root: PathBuf,
}

impl FilesystemBlobStore {
    /// Constructs a store at an absolute existing directory.
    pub fn try_new(root: PathBuf) -> Result<Self, FilesystemBlobStoreConstructionError> {
        if !root.is_absolute() {
            return Err(FilesystemBlobStoreConstructionError::NotAbsolute { root });
        }
        let metadata = std::fs::metadata(&root).map_err(|source| {
            FilesystemBlobStoreConstructionError::Inspect {
                root: root.clone(),
                source,
            }
        })?;
        if !metadata.is_dir() {
            return Err(FilesystemBlobStoreConstructionError::NotDirectory { root });
        }
        Ok(Self { root })
    }

    fn path(&self, key: &BlobObjectKey) -> PathBuf {
        self.root.join(key.as_str())
    }

    async fn put_inner(
        &self,
        expected: ExpectedBlob,
        mut source: BlobReader,
    ) -> Result<BlobPutOutcome, BlobStoreError> {
        let key = BlobObjectKey::for_digest(expected.digest());
        let destination = self.path(&key);
        if tokio::fs::try_exists(&destination)
            .await
            .map_err(|source| BlobStoreError::io("inspect destination", source))?
        {
            verify_file(&destination, expected).await?;
            return Ok(BlobPutOutcome::AlreadyPresent { key });
        }

        let parent = self.ensure_destination_parent(&key).await?;
        let temporary = NamedTempFile::new_in(&parent)
            .map_err(|source| BlobStoreError::io("create temporary object", source))?;
        let standard_file = temporary
            .reopen()
            .map_err(|source| BlobStoreError::io("open temporary object", source))?;
        let mut output = tokio::fs::File::from_std(standard_file);
        let mut digest = Sha256::new();
        let mut observed_length = 0_u64;
        let mut buffer = vec![0_u8; STREAM_BUFFER_BYTES];
        loop {
            let read = source
                .read(&mut buffer)
                .await
                .map_err(|source| BlobStoreError::io("read publication source", source))?;
            if read == 0 {
                break;
            }
            observed_length = observed_length
                .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
                .ok_or_else(|| {
                    BlobStoreError::verification(
                        "count publication bytes",
                        BlobVerificationFailure::new(expected, None, u64::MAX),
                    )
                })?;
            if observed_length > expected.byte_length() {
                return Err(BlobStoreError::verification(
                    "verify publication source",
                    BlobVerificationFailure::new(expected, None, observed_length),
                ));
            }
            digest.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .await
                .map_err(|source| BlobStoreError::io("write temporary object", source))?;
        }
        let observed_digest = BlobDigest::from_bytes(digest.finalize().into());
        if observed_length != expected.byte_length() || observed_digest != expected.digest() {
            return Err(BlobStoreError::verification(
                "verify publication source",
                BlobVerificationFailure::new(expected, Some(observed_digest), observed_length),
            ));
        }
        output
            .flush()
            .await
            .map_err(|source| BlobStoreError::io("flush temporary object", source))?;
        output
            .sync_all()
            .await
            .map_err(|source| BlobStoreError::io("sync temporary object", source))?;
        drop(output);

        let destination_for_publish = destination.clone();
        let publication = tokio::task::spawn_blocking(move || {
            temporary.persist_noclobber(&destination_for_publish)
        })
        .await
        .map_err(|source| BlobStoreError::io("join atomic publication", source))?;
        match publication {
            Ok(_) => {
                sync_directory(parent).await?;
                verify_file(&destination, expected).await?;
                Ok(BlobPutOutcome::Published { key })
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                verify_file(&destination, expected).await?;
                Ok(BlobPutOutcome::AlreadyPresent { key })
            }
            Err(error) => Err(BlobStoreError::io("atomically publish object", error.error)),
        }
    }

    async fn open_inner(&self, key: &BlobObjectKey) -> Result<OpenedBlob, BlobStoreError> {
        let path = self.path(key);
        let file = tokio::fs::File::open(&path).await.map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                BlobStoreError::not_found("open object")
            } else {
                BlobStoreError::io("open object", source)
            }
        })?;
        let metadata = file
            .metadata()
            .await
            .map_err(|source| BlobStoreError::io("inspect object", source))?;
        Ok(OpenedBlob::new(metadata.len(), Box::new(file)))
    }

    async fn ensure_destination_parent(
        &self,
        key: &BlobObjectKey,
    ) -> Result<PathBuf, BlobStoreError> {
        let relative_parent = std::path::Path::new(key.as_str())
            .parent()
            .ok_or_else(|| BlobStoreError::unavailable("derive destination parent"))?;
        let mut current = self.root.clone();
        for component in relative_parent.components() {
            let std::path::Component::Normal(segment) = component else {
                return Err(BlobStoreError::unavailable("resolve destination parent"));
            };
            let next = current.join(segment);
            match tokio::fs::create_dir(&next).await {
                Ok(()) => sync_directory(current.clone()).await?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata = tokio::fs::metadata(&next).await.map_err(|source| {
                        BlobStoreError::io("inspect destination directory", source)
                    })?;
                    if !metadata.is_dir() {
                        return Err(BlobStoreError::unavailable("use destination directory"));
                    }
                }
                Err(source) => {
                    return Err(BlobStoreError::io("create destination directory", source));
                }
            }
            current = next;
        }
        Ok(current)
    }
}

impl BlobStore for FilesystemBlobStore {
    fn put<'a>(
        &'a self,
        expected: ExpectedBlob,
        source: BlobReader,
    ) -> BlobStoreFuture<'a, BlobPutOutcome> {
        Box::pin(self.put_inner(expected, source))
    }

    fn open<'a>(&'a self, key: &'a BlobObjectKey) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(self.open_inner(key))
    }
}

async fn verify_file(path: &PathBuf, expected: ExpectedBlob) -> Result<(), BlobStoreError> {
    let mut file = tokio::fs::File::open(path).await.map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            BlobStoreError::not_found("verify object")
        } else {
            BlobStoreError::io("verify object", source)
        }
    })?;
    let mut digest = Sha256::new();
    let mut observed_length = 0_u64;
    let mut buffer = vec![0_u8; STREAM_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|source| BlobStoreError::io("read object for verification", source))?;
        if read == 0 {
            break;
        }
        observed_length = observed_length
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| {
                BlobStoreError::verification(
                    "count verified object bytes",
                    BlobVerificationFailure::new(expected, None, u64::MAX),
                )
            })?;
        if observed_length > expected.byte_length() {
            return Err(BlobStoreError::verification(
                "verify object",
                BlobVerificationFailure::new(expected, None, observed_length),
            ));
        }
        digest.update(&buffer[..read]);
    }
    let observed_digest = BlobDigest::from_bytes(digest.finalize().into());
    if observed_length == expected.byte_length() && observed_digest == expected.digest() {
        Ok(())
    } else {
        Err(BlobStoreError::verification(
            "verify object",
            BlobVerificationFailure::new(expected, Some(observed_digest), observed_length),
        ))
    }
}

async fn sync_directory(path: PathBuf) -> Result<(), BlobStoreError> {
    let path_for_sync = path.clone();
    tokio::task::spawn_blocking(move || {
        let directory = std::fs::File::open(&path_for_sync)?;
        directory.sync_all()
    })
    .await
    .map_err(|source| BlobStoreError::io("join directory sync", source))?
    .map_err(|source| BlobStoreError::io("sync destination directory", source))
}

/// Why a filesystem store root was rejected.
#[derive(Debug)]
pub enum FilesystemBlobStoreConstructionError {
    /// The configured root was not absolute.
    NotAbsolute { root: PathBuf },
    /// The configured root could not be inspected.
    Inspect { root: PathBuf, source: io::Error },
    /// The configured root was not a directory.
    NotDirectory { root: PathBuf },
}

impl std::fmt::Display for FilesystemBlobStoreConstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAbsolute { root } => write!(
                formatter,
                "filesystem blob-store root is not absolute: {}",
                root.display()
            ),
            Self::Inspect { root, .. } => write!(
                formatter,
                "filesystem blob-store root cannot be inspected: {}",
                root.display()
            ),
            Self::NotDirectory { root } => write!(
                formatter,
                "filesystem blob-store root is not a directory: {}",
                root.display()
            ),
        }
    }
}

impl std::error::Error for FilesystemBlobStoreConstructionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Inspect { source, .. } => Some(source),
            Self::NotAbsolute { .. } | Self::NotDirectory { .. } => None,
        }
    }
}
