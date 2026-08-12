//! Same-filesystem atomic immutable-blob publication.
//!
//! The normative specification is `docs/spec/blob-storage.md`.

use std::{
    ffi::{CString, OsStr, OsString},
    fmt::Write as _,
    fs, io,
    mem::MaybeUninit,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use rustix::fs::{
    AtFlags, Mode, OFlags, RawDir, RenameFlags, chmodat, fchmod, mkdirat, open, openat, renameat,
    renameat_with, unlinkat,
};
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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const STREAM_BUFFER_BYTES: usize = 64 * 1024;
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const PUBLICATION_DIRECTORY: &str = ".publish-v1";
const NAMESPACE_MARKER: &str = ".signalbox-blob-namespace-v1";
const TEMPORARY_NAME_ATTEMPTS: usize = 16;

struct TemporaryBlobFile {
    directory: Arc<fs::File>,
    name: OsString,
    linked: bool,
}

impl TemporaryBlobFile {
    fn publish_noclobber(
        mut self,
        destination_directory: &fs::File,
        destination_name: &OsStr,
    ) -> io::Result<Option<Self>> {
        match renameat_with(
            &self.directory,
            &self.name,
            destination_directory,
            destination_name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {
                self.linked = false;
                Ok(None)
            }
            Err(source) if source == rustix::io::Errno::EXIST => Ok(Some(self)),
            Err(source) => Err(io::Error::from(source)),
        }
    }

    fn replace(
        mut self,
        destination_directory: &fs::File,
        destination_name: &OsStr,
    ) -> io::Result<()> {
        renameat(
            &self.directory,
            &self.name,
            destination_directory,
            destination_name,
        )
        .map_err(io::Error::from)?;
        self.linked = false;
        Ok(())
    }

    fn remove(mut self) -> io::Result<()> {
        unlinkat(&self.directory, &self.name, AtFlags::empty()).map_err(io::Error::from)?;
        self.linked = false;
        Ok(())
    }
}

impl Drop for TemporaryBlobFile {
    fn drop(&mut self) {
        if self.linked {
            let _ = unlinkat(&self.directory, &self.name, AtFlags::empty());
        }
    }
}

/// Filesystem store rooted at one deployment-owned storage namespace.
#[derive(Clone)]
pub struct FilesystemBlobStore {
    root: Arc<fs::File>,
    publication_directory: Arc<fs::File>,
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
        let publication_directory = Arc::new(
            prepare_publication_directory(&root_descriptor).map_err(|source| {
                FilesystemBlobStoreConstructionError::PreparePublicationDirectory {
                    root: root.clone(),
                    source,
                }
            })?,
        );
        Ok(Self {
            root: root_descriptor,
            publication_directory,
        })
    }

    async fn put_inner(
        &self,
        expected: ExpectedBlob,
        mut source: BlobReader,
    ) -> Result<BlobPutOutcome, BlobStoreError> {
        let key = BlobObjectKey::for_digest(expected.digest());
        if !filesystem_key_is_admitted(&key) {
            return Err(BlobStoreError::unavailable("reject reserved object key"));
        }
        let destination_name = object_file_name(&key)
            .ok_or_else(|| BlobStoreError::unavailable("derive destination object name"))?;
        let parent = self.ensure_destination_parent(&key).await?;
        let repair_destination =
            if private_regular_file_exists_in_directory(parent.clone(), destination_name.clone())
                .await?
            {
                match verify_file_in_parent(parent.clone(), destination_name.clone(), expected)
                    .await
                {
                    Ok(()) => {
                        sync_open_directory(parent).await?;
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

        let publication_directory = self.publication_directory.clone();
        let (temporary, standard_file) =
            tokio::task::spawn_blocking(move || create_temporary_blob_file(publication_directory))
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
                self.publication_directory.clone(),
                parent,
                destination_name,
                key,
                expected,
            )
            .await;
        }

        let parent_for_publish = parent.clone();
        let destination_for_publish = destination_name.clone();
        let publication = tokio::task::spawn_blocking(move || {
            temporary.publish_noclobber(&parent_for_publish, &destination_for_publish)
        })
        .await
        .map_err(|source| BlobStoreError::io("join atomic publication", source))?;
        match publication {
            Ok(None) => {
                sync_open_directory(self.publication_directory.clone()).await?;
                sync_open_directory(parent.clone()).await?;
                verify_file_in_parent(parent, destination_name, expected).await?;
                Ok(BlobPutOutcome::Published { key })
            }
            Ok(Some(temporary)) => {
                match verify_file_in_parent(parent.clone(), destination_name.clone(), expected)
                    .await
                {
                    Ok(()) => {
                        remove_temporary_file(temporary).await?;
                        sync_open_directory(self.publication_directory.clone()).await?;
                        sync_open_directory(parent).await?;
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
                            temporary,
                            self.publication_directory.clone(),
                            parent,
                            destination_name,
                            key,
                            expected,
                        )
                        .await
                    }
                    Err(verification) => Err(verification),
                }
            }
            Err(error) => Err(BlobStoreError::io("atomically publish object", error)),
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
    ) -> Result<Arc<fs::File>, BlobStoreError> {
        let relative_parent = std::path::Path::new(key.as_str())
            .parent()
            .ok_or_else(|| BlobStoreError::unavailable("derive destination parent"))?;
        let mut current = self.root.clone();
        for component in relative_parent.components() {
            let std::path::Component::Normal(segment) = component else {
                return Err(BlobStoreError::unavailable("resolve destination parent"));
            };
            let segment = segment.to_os_string();
            let parent = current.clone();
            let next = tokio::task::spawn_blocking(move || {
                open_or_create_private_child_directory(&parent, &segment)
            })
            .await
            .map_err(|source| BlobStoreError::io("join destination directory creation", source))?
            .map_err(|source| BlobStoreError::io("create destination directory", source))?;
            sync_open_directory(current).await?;
            current = Arc::new(next);
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
    temporary: TemporaryBlobFile,
    publication_directory: Arc<fs::File>,
    parent: Arc<fs::File>,
    destination_name: OsString,
    key: BlobObjectKey,
    expected: ExpectedBlob,
) -> Result<BlobPutOutcome, BlobStoreError> {
    let parent_for_publish = parent.clone();
    let destination_for_publish = destination_name.clone();
    tokio::task::spawn_blocking(move || {
        temporary.replace(&parent_for_publish, &destination_for_publish)
    })
    .await
    .map_err(|source| BlobStoreError::io("join atomic repair", source))?
    .map_err(|source| BlobStoreError::io("atomically repair object", source))?;
    sync_open_directory(publication_directory).await?;
    sync_open_directory(parent.clone()).await?;
    verify_file_in_parent(parent, destination_name, expected).await?;
    Ok(BlobPutOutcome::Repaired { key })
}

async fn verify_file_in_parent(
    parent: Arc<fs::File>,
    destination_name: OsString,
    expected: ExpectedBlob,
) -> Result<(), BlobStoreError> {
    let (mut file, _) =
        open_private_regular_file_in_directory(parent, destination_name, "verify published object")
            .await?;
    verify_opened_file(&mut file, expected, "verify published object").await
}

async fn verify_opened_file(
    file: &mut tokio::fs::File,
    expected: ExpectedBlob,
    operation: &'static str,
) -> Result<(), BlobStoreError> {
    let mut digest = Sha256::new();
    let mut observed_length = 0_u64;
    let mut buffer = vec![0_u8; STREAM_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|source| BlobStoreError::io(operation, source))?;
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
                operation,
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
            operation,
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

async fn sync_open_directory(directory: Arc<fs::File>) -> Result<(), BlobStoreError> {
    tokio::task::spawn_blocking(move || directory.sync_all())
        .await
        .map_err(|source| BlobStoreError::io("join directory sync", source))?
        .map_err(|source| BlobStoreError::io("sync destination directory", source))
}

async fn remove_temporary_file(temporary: TemporaryBlobFile) -> Result<(), BlobStoreError> {
    tokio::task::spawn_blocking(move || temporary.remove())
        .await
        .map_err(|source| BlobStoreError::io("join temporary object cleanup", source))?
        .map_err(|source| BlobStoreError::io("remove temporary object", source))
}

async fn private_regular_file_exists_in_directory(
    directory: Arc<fs::File>,
    name: OsString,
) -> Result<bool, BlobStoreError> {
    match open_private_regular_file_in_directory(directory, name, "inspect destination object")
        .await
    {
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

async fn open_private_regular_file_in_directory(
    directory: Arc<fs::File>,
    name: OsString,
    operation: &'static str,
) -> Result<(tokio::fs::File, u64), BlobStoreError> {
    tokio::task::spawn_blocking(move || {
        let file = open_relative_regular_name(&directory, &name)?;
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
                || component == std::path::Component::Normal(std::ffi::OsStr::new(NAMESPACE_MARKER))
        })
}

fn object_file_name(key: &BlobObjectKey) -> Option<OsString> {
    Path::new(key.as_str()).file_name().map(OsStr::to_os_string)
}

fn create_temporary_blob_file(
    directory: Arc<fs::File>,
) -> io::Result<(TemporaryBlobFile, fs::File)> {
    for _attempt in 0..TEMPORARY_NAME_ATTEMPTS {
        let name = random_temporary_name()?;
        let file = match openat(
            &directory,
            &name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(file) => fs::File::from(file),
            Err(source) if source == rustix::io::Errno::EXIST => continue,
            Err(source) => return Err(io::Error::from(source)),
        };
        let temporary = TemporaryBlobFile {
            directory,
            name,
            linked: true,
        };
        fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(io::Error::from)?;
        if !private_regular_file_metadata(&file.metadata()?) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary blob object is not a private regular file",
            ));
        }
        let output = file.try_clone()?;
        return Ok((temporary, output));
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "temporary blob object names collided",
    ))
}

fn random_temporary_name() -> io::Result<OsString> {
    let mut random = [0_u8; 16];
    let mut filled = 0;
    while filled < random.len() {
        let byte_count =
            rustix::rand::getrandom(&mut random[filled..], rustix::rand::GetRandomFlags::empty())
                .map_err(io::Error::from)?;
        if byte_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "random source returned no temporary-name bytes",
            ));
        }
        filled += byte_count;
    }
    let mut name = String::with_capacity(37);
    name.push_str(".tmp-");
    for byte in random {
        write!(&mut name, "{byte:02x}").map_err(io::Error::other)?;
    }
    Ok(OsString::from(name))
}

#[cfg(target_os = "linux")]
fn open_or_create_private_child_directory(
    parent: &fs::File,
    segment: &std::ffi::OsStr,
) -> io::Result<fs::File> {
    let created = match mkdirat(parent, segment, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => true,
        Err(source) if source == rustix::io::Errno::EXIST => false,
        Err(source) => return Err(io::Error::from(source)),
    };
    if created {
        chmodat(
            parent,
            segment,
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
            AtFlags::empty(),
        )
        .map_err(io::Error::from)?;
    }
    let directory = openat2(
        parent,
        segment,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )
    .map(fs::File::from)
    .map_err(io::Error::from)?;
    if !private_directory_metadata(&directory.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "blob directory is not private",
        ));
    }
    Ok(directory)
}

#[cfg(not(target_os = "linux"))]
fn open_or_create_private_child_directory(
    _parent: &fs::File,
    _segment: &std::ffi::OsStr,
) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem blob storage requires Linux descriptor-relative directories",
    ))
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

#[cfg(target_os = "linux")]
fn open_relative_regular_name(directory: &fs::File, name: &OsStr) -> io::Result<fs::File> {
    let object = openat2(
        directory,
        name,
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
        openat(
            root,
            key.as_str(),
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    ))
}

#[cfg(not(target_os = "linux"))]
fn open_relative_regular_name(directory: &fs::File, name: &OsStr) -> io::Result<fs::File> {
    Ok(fs::File::from(
        openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    ))
}

#[cfg(target_os = "linux")]
fn prepare_publication_directory(root: &fs::File) -> io::Result<fs::File> {
    let created = match mkdirat(
        root,
        PUBLICATION_DIRECTORY,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    ) {
        Ok(()) => true,
        Err(source) if source == rustix::io::Errno::EXIST => false,
        Err(source) => return Err(io::Error::from(source)),
    };
    if created {
        chmodat(
            root,
            PUBLICATION_DIRECTORY,
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
            AtFlags::empty(),
        )
        .map_err(io::Error::from)?;
    }
    let publication = fs::File::from(
        openat2(
            root,
            PUBLICATION_DIRECTORY,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(io::Error::from)?,
    );
    if !private_directory_metadata(&publication.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "blob publication directory is not private",
        ));
    }
    root.sync_all()?;
    sweep_publication_directory(&publication)?;
    publication.sync_all()?;
    Ok(publication)
}

#[cfg(not(target_os = "linux"))]
fn prepare_publication_directory(_root: &fs::File) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem blob storage requires Linux descriptor-relative publication",
    ))
}

#[cfg(target_os = "linux")]
fn sweep_publication_directory(directory: &fs::File) -> io::Result<()> {
    let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
    let mut entries = RawDir::new(directory, &mut buffer);
    while let Some(entry) = entries.next() {
        let name = CString::from(entry.map_err(io::Error::from)?.file_name());
        if name.as_bytes() == b"." || name.as_bytes() == b".." {
            continue;
        }
        let file = fs::File::from(
            openat2(
                directory,
                &name,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
            )
            .map_err(io::Error::from)?,
        );
        if !private_regular_file_metadata(&file.metadata()?) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "publication directory contains an unowned or non-regular entry",
            ));
        }
        unlinkat(directory, &name, AtFlags::empty()).map_err(io::Error::from)?;
    }
    Ok(())
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
        && metadata.nlink() == 1
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn inv059_filesystem_rejects_mounted_child_directories() {
        let root = fs::File::from(
            open(
                "/",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("the Linux root directory opens"),
        );

        let error = open_or_create_private_child_directory(&root, std::ffi::OsStr::new("proc"))
            .expect_err("the procfs mount must not be admitted below its parent mount");

        assert_eq!(error.kind(), io::ErrorKind::CrossesDevices);
    }

    #[test]
    fn inv059_filesystem_publishes_through_the_retained_parent_descriptor() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        let publication_path = root.path().join("publication");
        let destination_path = root.path().join("destination");
        let moved_destination = root.path().join("moved-destination");
        fs::create_dir(&publication_path).expect("the fixture creates a publication directory");
        fs::create_dir(&destination_path).expect("the fixture creates a destination directory");
        fs::set_permissions(
            &publication_path,
            fs::Permissions::from_mode(DIRECTORY_MODE),
        )
        .expect("the publication directory is private");
        fs::set_permissions(
            &destination_path,
            fs::Permissions::from_mode(DIRECTORY_MODE),
        )
        .expect("the destination directory is private");
        let publication = Arc::new(
            open(
                &publication_path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map(fs::File::from)
            .expect("the publication descriptor opens"),
        );
        let destination = fs::File::from(
            open(
                &destination_path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("the destination descriptor opens"),
        );
        let (temporary, mut output) = create_temporary_blob_file(publication)
            .expect("the fixture creates a descriptor-relative temporary file");
        output
            .write_all(b"descriptor-bound bytes")
            .expect("the fixture writes the temporary bytes");
        output
            .sync_all()
            .expect("the fixture synchronizes the temporary bytes");
        fs::rename(&destination_path, &moved_destination)
            .expect("the destination generation is renamed");
        fs::create_dir(&destination_path).expect("a replacement destination path is created");

        let outcome = temporary
            .publish_noclobber(&destination, OsStr::new("object"))
            .expect("publication through the retained descriptor succeeds");

        assert!(outcome.is_none());
        assert!(moved_destination.join("object").is_file());
        assert!(!destination_path.join("object").exists());
    }
}
