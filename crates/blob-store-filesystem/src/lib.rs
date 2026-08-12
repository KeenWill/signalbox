//! Same-filesystem atomic immutable-blob publication.
//!
//! The normative specification is `docs/spec/blob-storage.md`.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
use std::{
    os::fd::AsRawFd,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
};

use rustix::fs::{Mode, OFlags, open};
#[cfg(target_os = "linux")]
use rustix::fs::{ResolveFlags, openat2};
#[cfg(unix)]
use rustix::process::geteuid;
use sha2::{Digest, Sha256};
use signalbox_blob_store::{
    BlobObjectKey, BlobPutOutcome, BlobReader, BlobStore, BlobStoreError, BlobStoreFuture,
    BlobVerificationFailure, ExpectedBlob, MAX_BLOB_RANGE_BYTES, OpenedBlob,
};
use signalbox_domain::BlobDigest;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const STREAM_BUFFER_BYTES: usize = 64 * 1024;
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const PUBLICATION_DIRECTORY: &str = ".publish-v1";

/// Filesystem store rooted at one deployment-owned storage namespace.
#[derive(Clone)]
pub struct FilesystemBlobStore {
    root: Arc<fs::File>,
    root_directory: PathBuf,
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
        let root_descriptor = open(
            &root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(fs::File::from)
        .map_err(io::Error::from)
        .map_err(|source| FilesystemBlobStoreConstructionError::Inspect {
            root: root.clone(),
            source,
        })?;
        let metadata = root_descriptor.metadata().map_err(|source| {
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
        if !positively_classified_local_filesystem(&root_descriptor).map_err(|source| {
            FilesystemBlobStoreConstructionError::Inspect {
                root: root.clone(),
                source,
            }
        })? {
            return Err(FilesystemBlobStoreConstructionError::UnclassifiedFilesystem { root });
        }
        let root_descriptor = Arc::new(root_descriptor);
        let root_directory = pinned_directory_path(&root_descriptor);
        let publication_directory = root_directory.join(PUBLICATION_DIRECTORY);
        prepare_publication_directory(&root_descriptor, &root_directory, &publication_directory)
            .map_err(|source| {
                FilesystemBlobStoreConstructionError::PreparePublicationDirectory {
                    root: root.clone(),
                    source,
                }
            })?;
        Ok(Self {
            root: root_descriptor,
            root_directory,
            publication_directory,
        })
    }

    fn path(&self, key: &BlobObjectKey) -> Option<PathBuf> {
        filesystem_key_is_admitted(key).then(|| self.root_directory.join(key.as_str()))
    }

    async fn put_inner(
        &self,
        expected: ExpectedBlob,
        mut source: BlobReader,
    ) -> Result<BlobPutOutcome, BlobStoreError> {
        let key = BlobObjectKey::for_digest(expected.digest());
        let destination = self
            .path(&key)
            .ok_or_else(|| BlobStoreError::unavailable("reject reserved object key"))?;
        let repair_destination = if private_regular_file_exists(self.root.clone(), &key).await? {
            match verify_file(&self.root, &key, expected).await {
                Ok(()) => {
                    let parent = destination
                        .parent()
                        .ok_or_else(|| BlobStoreError::unavailable("derive destination parent"))?
                        .to_path_buf();
                    sync_directory(parent).await?;
                    return Ok(BlobPutOutcome::AlreadyPresent { key });
                }
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
        let publication_directory = self.publication_directory.clone();
        let (temporary, standard_file) = tokio::task::spawn_blocking(move || {
            let temporary = NamedTempFile::new_in(publication_directory)?;
            if !private_regular_file_metadata(&temporary.as_file().metadata()?) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "temporary blob object is not a private regular file",
                ));
            }
            let standard_file = temporary.reopen()?;
            Ok::<_, io::Error>((temporary, standard_file))
        })
        .await
        .map_err(|source| BlobStoreError::io("join temporary object creation", source))?
        .map_err(|source| BlobStoreError::io("create temporary object", source))?;
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
                self.root.clone(),
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
                verify_file(&self.root, &key, expected).await?;
                Ok(BlobPutOutcome::Published { key })
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                match verify_file(&self.root, &key, expected).await {
                    Ok(()) => {
                        drop(error.file);
                        sync_directory(self.publication_directory.clone()).await?;
                        sync_directory(parent).await?;
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
                            self.root.clone(),
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
        if !filesystem_key_is_admitted(key) {
            return Err(BlobStoreError::unavailable("reject reserved object key"));
        }
        let (file, byte_length) =
            open_private_regular_file(self.root.clone(), key.clone(), "open object").await?;
        Ok(OpenedBlob::new(byte_length, Box::new(file)))
    }

    async fn open_range_inner(
        &self,
        expected: ExpectedBlob,
        key: &BlobObjectKey,
        offset: u64,
        byte_length: std::num::NonZeroU64,
    ) -> Result<OpenedBlob, BlobStoreError> {
        if !filesystem_key_is_admitted(key) {
            return Err(BlobStoreError::unavailable("reject reserved object key"));
        }
        verify_and_retain_range(
            self.root.clone(),
            key.clone(),
            expected,
            offset,
            byte_length,
        )
        .await
    }

    async fn ensure_destination_parent(
        &self,
        key: &BlobObjectKey,
    ) -> Result<PathBuf, BlobStoreError> {
        let relative_parent = std::path::Path::new(key.as_str())
            .parent()
            .ok_or_else(|| BlobStoreError::unavailable("derive destination parent"))?;
        let mut current = self.root_directory.clone();
        let mut current_is_root = true;
        for component in relative_parent.components() {
            let std::path::Component::Normal(segment) = component else {
                return Err(BlobStoreError::unavailable("resolve destination parent"));
            };
            let next = current.join(segment);
            let next_for_creation = next.clone();
            tokio::task::spawn_blocking(move || {
                create_or_validate_private_directory(&next_for_creation)
            })
            .await
            .map_err(|source| BlobStoreError::io("join destination directory creation", source))?
            .map_err(|source| BlobStoreError::io("create destination directory", source))?;
            if current_is_root {
                sync_open_directory(self.root.clone()).await?;
            } else {
                sync_directory(current.clone()).await?;
            }
            current = next;
            current_is_root = false;
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
        expected: ExpectedBlob,
        key: &'a BlobObjectKey,
        offset: u64,
        byte_length: std::num::NonZeroU64,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(self.open_range_inner(expected, key, offset, byte_length))
    }
}

async fn replace_corrupt_destination(
    temporary: NamedTempFile,
    destination: PathBuf,
    parent: PathBuf,
    publication_directory: PathBuf,
    root: Arc<fs::File>,
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
    verify_file(&root, &key, expected).await?;
    Ok(BlobPutOutcome::Repaired { key })
}

async fn verify_file(
    root: &Arc<fs::File>,
    key: &BlobObjectKey,
    expected: ExpectedBlob,
) -> Result<(), BlobStoreError> {
    let (mut file, _) =
        open_private_regular_file(root.clone(), key.clone(), "verify object").await?;
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

async fn verify_and_retain_range(
    root: Arc<fs::File>,
    key: BlobObjectKey,
    expected: ExpectedBlob,
    offset: u64,
    byte_length: std::num::NonZeroU64,
) -> Result<OpenedBlob, BlobStoreError> {
    if byte_length.get() > MAX_BLOB_RANGE_BYTES {
        return Err(BlobStoreError::unavailable("validate object range"));
    }
    let end = offset
        .checked_add(byte_length.get())
        .filter(|end| *end <= expected.byte_length())
        .ok_or_else(|| BlobStoreError::unavailable("validate object range"))?;
    let (mut file, _) = open_private_regular_file(root, key, "open object range").await?;
    let retained_capacity = usize::try_from(byte_length.get())
        .map_err(|_| BlobStoreError::unavailable("allocate object range"))?;
    let mut retained = Vec::with_capacity(retained_capacity);
    let mut digest = Sha256::new();
    let mut observed_length = 0_u64;
    let mut buffer = vec![0_u8; STREAM_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|source| BlobStoreError::io("read object range verification", source))?;
        if read == 0 {
            break;
        }
        let chunk_start = observed_length;
        observed_length = observed_length
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| {
                BlobStoreError::verification(
                    "count range-verified object bytes",
                    BlobVerificationFailure::new(expected, None, u64::MAX),
                )
            })?;
        if observed_length > expected.byte_length() {
            return Err(BlobStoreError::verification(
                "verify object for range",
                BlobVerificationFailure::new(expected, None, observed_length),
            ));
        }
        let retain_start = offset.max(chunk_start);
        let retain_end = end.min(observed_length);
        if retain_start < retain_end {
            let buffer_start = usize::try_from(retain_start - chunk_start)
                .map_err(|_| BlobStoreError::unavailable("retain object range"))?;
            let buffer_end = usize::try_from(retain_end - chunk_start)
                .map_err(|_| BlobStoreError::unavailable("retain object range"))?;
            retained.extend_from_slice(&buffer[buffer_start..buffer_end]);
        }
        digest.update(&buffer[..read]);
    }
    let observed_digest = BlobDigest::from_bytes(digest.finalize().into());
    if observed_length != expected.byte_length() || observed_digest != expected.digest() {
        return Err(BlobStoreError::verification(
            "verify object for range",
            BlobVerificationFailure::new(expected, Some(observed_digest), observed_length),
        ));
    }
    if retained.len() != retained_capacity {
        return Err(BlobStoreError::unavailable("retain complete object range"));
    }
    Ok(OpenedBlob::new(
        byte_length.get(),
        Box::new(std::io::Cursor::new(retained)),
    ))
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

async fn sync_open_directory(directory: Arc<fs::File>) -> Result<(), BlobStoreError> {
    tokio::task::spawn_blocking(move || directory.sync_all())
        .await
        .map_err(|source| BlobStoreError::io("join directory sync", source))?
        .map_err(|source| BlobStoreError::io("sync destination directory", source))
}

async fn private_regular_file_exists(
    root: Arc<fs::File>,
    key: &BlobObjectKey,
) -> Result<bool, BlobStoreError> {
    match open_private_regular_file(root, key.clone(), "inspect destination object").await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == signalbox_blob_store::BlobStoreFailureKind::NotFound => {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

async fn open_private_regular_file(
    root: Arc<fs::File>,
    key: BlobObjectKey,
    operation: &'static str,
) -> Result<(tokio::fs::File, u64), BlobStoreError> {
    tokio::task::spawn_blocking(move || {
        let file = open_relative_regular_candidate(&root, &key)?;
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

fn filesystem_key_is_admitted(key: &BlobObjectKey) -> bool {
    !Path::new(key.as_str())
        .components()
        .next()
        .is_some_and(|component| {
            component == std::path::Component::Normal(std::ffi::OsStr::new(PUBLICATION_DIRECTORY))
        })
}

#[cfg(target_os = "linux")]
fn open_relative_regular_candidate(root: &fs::File, key: &BlobObjectKey) -> io::Result<fs::File> {
    let object = openat2(
        root,
        key.as_str(),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )
    .map_err(io::Error::from)?;
    Ok(fs::File::from(object))
}

#[cfg(not(target_os = "linux"))]
fn open_relative_regular_candidate(root: &fs::File, key: &BlobObjectKey) -> io::Result<fs::File> {
    Ok(fs::File::from(
        open(
            pinned_directory_path(root).join(key.as_str()),
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    ))
}

fn prepare_publication_directory(
    root: &fs::File,
    root_directory: &Path,
    publication_directory: &Path,
) -> io::Result<()> {
    create_or_validate_private_directory(publication_directory)?;
    root.sync_all()?;
    validate_publication_directory_mount(root, root_directory)?;
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
    #[cfg(unix)]
    if created {
        fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE))?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !private_directory_metadata(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "blob directory is not private",
        ));
    }
    Ok(created)
}

#[cfg(target_os = "linux")]
fn validate_publication_directory_mount(root: &fs::File, _root_path: &Path) -> io::Result<()> {
    let publication = openat2(
        root,
        PUBLICATION_DIRECTORY,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )
    .map_err(io::Error::from)?;
    drop(publication);
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn validate_publication_directory_mount(root: &fs::File, root_path: &Path) -> io::Result<()> {
    let publication_directory = root_path.join(PUBLICATION_DIRECTORY);
    if root.metadata()?.dev() == fs::symlink_metadata(publication_directory)?.dev() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication directory is on another filesystem",
        ))
    }
}

#[cfg(not(unix))]
fn validate_publication_directory_mount(_root: &fs::File, _root_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem blob storage requires Unix mount identity",
    ))
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
fn positively_classified_local_filesystem(root: &fs::File) -> io::Result<bool> {
    const EXT_SUPER_MAGIC: u32 = 0x0000_ef53;
    const XFS_SUPER_MAGIC: u32 = 0x5846_5342;
    const BTRFS_SUPER_MAGIC: u32 = 0x9123_683e;
    const ZFS_SUPER_MAGIC: u32 = 0x2fc1_2fc1;
    const F2FS_SUPER_MAGIC: u32 = 0xf2f5_2010;
    let filesystem = rustix::fs::fstatfs(root).map_err(io::Error::from)?.f_type as u32;
    Ok(matches!(
        filesystem,
        EXT_SUPER_MAGIC | XFS_SUPER_MAGIC | BTRFS_SUPER_MAGIC | ZFS_SUPER_MAGIC | F2FS_SUPER_MAGIC
    ))
}

#[cfg(not(target_os = "linux"))]
fn positively_classified_local_filesystem(_root: &fs::File) -> io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
fn pinned_directory_path(directory: &fs::File) -> PathBuf {
    PathBuf::from("/proc/self/fd").join(directory.as_raw_fd().to_string())
}

#[cfg(not(unix))]
fn pinned_directory_path(_directory: &fs::File) -> PathBuf {
    PathBuf::new()
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
