//! Same-filesystem atomic immutable-blob publication.
//!
//! The normative specification is `docs/spec/blob-storage.md`.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt};

use rustix::fs::{Mode, OFlags, open};
#[cfg(unix)]
use rustix::process::geteuid;
use sha2::{Digest, Sha256};
use signalbox_blob_store::{
    BlobObjectKey, BlobPutOutcome, BlobReader, BlobStore, BlobStoreError, BlobStoreFuture,
    BlobVerificationFailure, ExpectedBlob, OpenedBlob,
};
use signalbox_domain::BlobDigest;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const STREAM_BUFFER_BYTES: usize = 64 * 1024;
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const PUBLICATION_DIRECTORY: &str = ".publish-v1";

/// Filesystem store rooted at one deployment-owned storage namespace.
#[derive(Clone)]
pub struct FilesystemBlobStore {
    root: PathBuf,
    publication_directory: PathBuf,
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
        let metadata = fs::symlink_metadata(&root).map_err(|source| {
            FilesystemBlobStoreConstructionError::Inspect {
                root: root.clone(),
                source,
            }
        })?;
        if !metadata.is_dir() {
            return Err(FilesystemBlobStoreConstructionError::NotDirectory { root });
        }
        if !private_directory_metadata(&metadata) {
            return Err(FilesystemBlobStoreConstructionError::NotPrivate { root });
        }
        if !positively_classified_local_filesystem(&root).map_err(|source| {
            FilesystemBlobStoreConstructionError::Inspect {
                root: root.clone(),
                source,
            }
        })? {
            return Err(FilesystemBlobStoreConstructionError::UnclassifiedFilesystem { root });
        }
        let publication_directory = root.join(PUBLICATION_DIRECTORY);
        prepare_publication_directory(&root, &publication_directory).map_err(|source| {
            FilesystemBlobStoreConstructionError::PreparePublicationDirectory {
                root: root.clone(),
                source,
            }
        })?;
        Ok(Self {
            root,
            publication_directory,
        })
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
        let repair_destination = if private_regular_file_exists(&destination).await? {
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
        let temporary = NamedTempFile::new_in(&self.publication_directory)
            .map_err(|source| BlobStoreError::io("create temporary object", source))?;
        if !private_regular_file_metadata(
            &temporary
                .as_file()
                .metadata()
                .map_err(|source| BlobStoreError::io("inspect temporary object", source))?,
        ) {
            return Err(BlobStoreError::unavailable("validate temporary object"));
        }
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
            return replace_corrupt_destination(
                temporary,
                destination,
                parent,
                self.publication_directory.clone(),
                key,
                expected,
            )
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
                sync_directory(self.publication_directory.clone()).await?;
                sync_directory(parent).await?;
                verify_file(&destination, expected).await?;
                Ok(BlobPutOutcome::Published { key })
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                match verify_file(&destination, expected).await {
                    Ok(()) => {
                        drop(error.file);
                        sync_directory(self.publication_directory.clone()).await?;
                        Ok(BlobPutOutcome::AlreadyPresent { key })
                    }
                    Err(verification)
                        if matches!(
                            verification.kind(),
                            signalbox_blob_store::BlobStoreFailureKind::NotFound
                                | signalbox_blob_store::BlobStoreFailureKind::VerificationFailed
                        ) =>
                    {
                        replace_corrupt_destination(
                            error.file,
                            destination,
                            parent,
                            self.publication_directory.clone(),
                            key,
                            expected,
                        )
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
        let (file, byte_length) = open_private_regular_file(path, "open object").await?;
        Ok(OpenedBlob::new(byte_length, Box::new(file)))
    }

    async fn open_range_inner(
        &self,
        key: &BlobObjectKey,
        offset: u64,
        byte_length: std::num::NonZeroU64,
    ) -> Result<OpenedBlob, BlobStoreError> {
        let path = self.path(key);
        let (mut file, stored_length) =
            open_private_regular_file(path, "open object range").await?;
        let end = offset
            .checked_add(byte_length.get())
            .ok_or_else(|| BlobStoreError::unavailable("validate object range"))?;
        if end > stored_length {
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
            let next_for_creation = next.clone();
            let created = tokio::task::spawn_blocking(move || {
                create_or_validate_private_directory(&next_for_creation)
            })
            .await
            .map_err(|source| BlobStoreError::io("join destination directory creation", source))?
            .map_err(|source| BlobStoreError::io("create destination directory", source))?;
            if created {
                sync_directory(current.clone()).await?;
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
    publication_directory: PathBuf,
    key: BlobObjectKey,
    expected: ExpectedBlob,
) -> Result<BlobPutOutcome, BlobStoreError> {
    let destination_for_publish = destination.clone();
    let persisted = tokio::task::spawn_blocking(move || temporary.persist(destination_for_publish))
        .await
        .map_err(|source| BlobStoreError::io("join atomic repair", source))?
        .map_err(|error| BlobStoreError::io("atomically repair object", error.error))?;
    drop(persisted);
    sync_directory(publication_directory).await?;
    sync_directory(parent).await?;
    verify_file(&destination, expected).await?;
    Ok(BlobPutOutcome::Repaired { key })
}

async fn verify_file(path: &Path, expected: ExpectedBlob) -> Result<(), BlobStoreError> {
    let (mut file, _) = open_private_regular_file(path.to_path_buf(), "verify object").await?;
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
    tokio::task::spawn_blocking(move || {
        let directory = fs::File::from(
            open(
                &path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        directory.sync_all()
    })
    .await
    .map_err(|source| BlobStoreError::io("join directory sync", source))?
    .map_err(|source| BlobStoreError::io("sync destination directory", source))
}

async fn private_regular_file_exists(path: &Path) -> Result<bool, BlobStoreError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if private_regular_file_metadata(&metadata) => Ok(true),
        Ok(_) => Err(BlobStoreError::unavailable("validate destination object")),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(BlobStoreError::io("inspect destination object", source)),
    }
}

async fn open_private_regular_file(
    path: PathBuf,
    operation: &'static str,
) -> Result<(tokio::fs::File, u64), BlobStoreError> {
    tokio::task::spawn_blocking(move || {
        let file = fs::File::from(
            open(
                &path,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        let metadata = file.metadata()?;
        if !private_regular_file_metadata(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "blob object is not a private regular file",
            ));
        }
        Ok((tokio::fs::File::from_std(file), metadata.len()))
    })
    .await
    .map_err(|source| BlobStoreError::io("join private object open", source))?
    .map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            BlobStoreError::not_found(operation)
        } else {
            BlobStoreError::io(operation, source)
        }
    })
}

fn prepare_publication_directory(root: &Path, publication_directory: &Path) -> io::Result<()> {
    let created = create_or_validate_private_directory(publication_directory)?;
    if created {
        sync_directory_blocking(root)?;
    }
    for entry in fs::read_dir(publication_directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !private_regular_file_metadata(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "publication directory contains an unowned or non-regular entry",
            ));
        }
        fs::remove_file(entry.path())?;
    }
    sync_directory_blocking(publication_directory)
}

fn create_or_validate_private_directory(path: &Path) -> io::Result<bool> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(DIRECTORY_MODE);
    let created = match builder.create(path) {
        Ok(()) => true,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => false,
        Err(source) => return Err(source),
    };
    let metadata = fs::symlink_metadata(path)?;
    if !private_directory_metadata(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "blob directory is not private",
        ));
    }
    Ok(created)
}

fn sync_directory_blocking(path: &Path) -> io::Result<()> {
    let directory = fs::File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    directory.sync_all()
}

#[cfg(unix)]
fn private_directory_metadata(metadata: &fs::Metadata) -> bool {
    metadata.is_dir()
        && metadata.uid() == geteuid().as_raw()
        && metadata.mode() & PERMISSION_MASK == DIRECTORY_MODE
}

#[cfg(not(unix))]
fn private_directory_metadata(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn private_regular_file_metadata(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
        && metadata.uid() == geteuid().as_raw()
        && metadata.mode() & PERMISSION_MASK == FILE_MODE
}

#[cfg(not(unix))]
fn private_regular_file_metadata(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn positively_classified_local_filesystem(path: &Path) -> io::Result<bool> {
    const EXT_SUPER_MAGIC: u32 = 0x0000_ef53;
    const XFS_SUPER_MAGIC: u32 = 0x5846_5342;
    const BTRFS_SUPER_MAGIC: u32 = 0x9123_683e;
    const TMPFS_MAGIC: u32 = 0x0102_1994;
    const OVERLAYFS_SUPER_MAGIC: u32 = 0x794c_7630;
    const ZFS_SUPER_MAGIC: u32 = 0x2fc1_2fc1;
    const F2FS_SUPER_MAGIC: u32 = 0xf2f5_2010;
    const RAMFS_MAGIC: u32 = 0x8584_58f6;
    let filesystem = rustix::fs::statfs(path).map_err(io::Error::from)?.f_type as u32;
    Ok(matches!(
        filesystem,
        EXT_SUPER_MAGIC
            | XFS_SUPER_MAGIC
            | BTRFS_SUPER_MAGIC
            | TMPFS_MAGIC
            | OVERLAYFS_SUPER_MAGIC
            | ZFS_SUPER_MAGIC
            | F2FS_SUPER_MAGIC
            | RAMFS_MAGIC
    ))
}

#[cfg(not(target_os = "linux"))]
fn positively_classified_local_filesystem(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

/// Why a filesystem store root was rejected.
pub enum FilesystemBlobStoreConstructionError {
    /// The configured root was not absolute.
    NotAbsolute { root: PathBuf },
    /// The configured root could not be inspected.
    Inspect { root: PathBuf, source: io::Error },
    /// The configured root was not a directory.
    NotDirectory { root: PathBuf },
    /// The configured root was not owned by the effective user with mode 0700.
    NotPrivate { root: PathBuf },
    /// The host could not positively classify the root as local kernel storage.
    UnclassifiedFilesystem { root: PathBuf },
    /// The private crash-recovery publication directory could not be prepared.
    PreparePublicationDirectory { root: PathBuf, source: io::Error },
}

impl std::fmt::Debug for FilesystemBlobStoreConstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotAbsolute { .. } => "FilesystemBlobStoreConstructionError::NotAbsolute",
            Self::Inspect { .. } => "FilesystemBlobStoreConstructionError::Inspect",
            Self::NotDirectory { .. } => "FilesystemBlobStoreConstructionError::NotDirectory",
            Self::NotPrivate { .. } => "FilesystemBlobStoreConstructionError::NotPrivate",
            Self::UnclassifiedFilesystem { .. } => {
                "FilesystemBlobStoreConstructionError::UnclassifiedFilesystem"
            }
            Self::PreparePublicationDirectory { .. } => {
                "FilesystemBlobStoreConstructionError::PreparePublicationDirectory"
            }
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
            Self::NotPrivate { .. } => {
                formatter.write_str("filesystem blob-store root is not private")
            }
            Self::UnclassifiedFilesystem { .. } => formatter
                .write_str("filesystem blob-store root is not positively classified as local"),
            Self::PreparePublicationDirectory { .. } => formatter
                .write_str("filesystem blob-store publication directory cannot be prepared"),
        }
    }
}

impl std::error::Error for FilesystemBlobStoreConstructionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Inspect { source, .. } | Self::PreparePublicationDirectory { source, .. } => {
                Some(source)
            }
            Self::NotAbsolute { .. }
            | Self::NotDirectory { .. }
            | Self::NotPrivate { .. }
            | Self::UnclassifiedFilesystem { .. } => None,
        }
    }
}
