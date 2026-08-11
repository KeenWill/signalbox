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
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const STREAM_BUFFER_BYTES: usize = 64 * 1024;

/// Filesystem store rooted at one deployment-owned storage namespace.
#[derive(Clone)]
pub struct FilesystemBlobStore {
    root: PathBuf,
}

impl std::fmt::Debug for FilesystemBlobStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FilesystemBlobStore { root: <redacted> }")
    }
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
        let repair_destination = if tokio::fs::try_exists(&destination)
            .await
            .map_err(|source| BlobStoreError::io("inspect destination", source))?
        {
            match verify_file(&destination, expected).await {
                Ok(()) => return Ok(BlobPutOutcome::AlreadyPresent { key }),
                Err(error)
                    if matches!(
                        error.kind(),
                        signalbox_blob_store::BlobStoreFailureKind::NotFound
                            | signalbox_blob_store::BlobStoreFailureKind::VerificationFailed
                    ) =>
                {
                    true
                }
                Err(error) => return Err(error),
            }
        } else {
            false
        };

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

        if repair_destination {
            return replace_corrupt_destination(temporary, destination, parent, key, expected)
                .await;
        }

        let destination_for_publish = destination.clone();
        let publication = tokio::task::spawn_blocking(move || {
            temporary.persist_noclobber(&destination_for_publish)
        })
        .await
        .map_err(|source| BlobStoreError::io("join atomic publication", source))?;
        match publication {
            Ok(persisted) => {
                drop(persisted);
                sync_directory(parent).await?;
                verify_file(&destination, expected).await?;
                Ok(BlobPutOutcome::Published { key })
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                match verify_file(&destination, expected).await {
                    Ok(()) => {
                        drop(error.file);
                        Ok(BlobPutOutcome::AlreadyPresent { key })
                    }
                    Err(verification)
                        if matches!(
                            verification.kind(),
                            signalbox_blob_store::BlobStoreFailureKind::NotFound
                                | signalbox_blob_store::BlobStoreFailureKind::VerificationFailed
                        ) =>
                    {
                        replace_corrupt_destination(error.file, destination, parent, key, expected)
                            .await
                    }
                    Err(verification) => Err(verification),
                }
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

    async fn open_range_inner(
        &self,
        key: &BlobObjectKey,
        offset: u64,
        byte_length: std::num::NonZeroU64,
    ) -> Result<OpenedBlob, BlobStoreError> {
        let path = self.path(key);
        let mut file = tokio::fs::File::open(&path).await.map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                BlobStoreError::not_found("open object range")
            } else {
                BlobStoreError::io("open object range", source)
            }
        })?;
        let metadata = file
            .metadata()
            .await
            .map_err(|source| BlobStoreError::io("inspect object range", source))?;
        let end = offset
            .checked_add(byte_length.get())
            .ok_or_else(|| BlobStoreError::unavailable("validate object range"))?;
        if end > metadata.len() {
            return Err(BlobStoreError::unavailable("validate object range"));
        }
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|source| BlobStoreError::io("seek object range", source))?;
        Ok(OpenedBlob::new(
            byte_length.get(),
            Box::new(file.take(byte_length.get())),
        ))
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

    fn open_range<'a>(
        &'a self,
        key: &'a BlobObjectKey,
        offset: u64,
        byte_length: std::num::NonZeroU64,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(self.open_range_inner(key, offset, byte_length))
    }
}

async fn replace_corrupt_destination(
    temporary: NamedTempFile,
    destination: PathBuf,
    parent: PathBuf,
    key: BlobObjectKey,
    expected: ExpectedBlob,
) -> Result<BlobPutOutcome, BlobStoreError> {
    let destination_for_publish = destination.clone();
    let persisted = tokio::task::spawn_blocking(move || temporary.persist(destination_for_publish))
        .await
        .map_err(|source| BlobStoreError::io("join atomic repair", source))?
        .map_err(|error| BlobStoreError::io("atomically repair object", error.error))?;
    drop(persisted);
    sync_directory(parent).await?;
    verify_file(&destination, expected).await?;
    Ok(BlobPutOutcome::Repaired { key })
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
pub enum FilesystemBlobStoreConstructionError {
    /// The configured root was not absolute.
    NotAbsolute { root: PathBuf },
    /// The configured root could not be inspected.
    Inspect { root: PathBuf, source: io::Error },
    /// The configured root was not a directory.
    NotDirectory { root: PathBuf },
}

impl std::fmt::Debug for FilesystemBlobStoreConstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotAbsolute { .. } => "FilesystemBlobStoreConstructionError::NotAbsolute",
            Self::Inspect { .. } => "FilesystemBlobStoreConstructionError::Inspect",
            Self::NotDirectory { .. } => "FilesystemBlobStoreConstructionError::NotDirectory",
        })
    }
}

impl std::fmt::Display for FilesystemBlobStoreConstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAbsolute { .. } => {
                formatter.write_str("filesystem blob-store root is not absolute")
            }
            Self::Inspect { .. } => {
                formatter.write_str("filesystem blob-store root cannot be inspected")
            }
            Self::NotDirectory { .. } => {
                formatter.write_str("filesystem blob-store root is not a directory")
            }
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
