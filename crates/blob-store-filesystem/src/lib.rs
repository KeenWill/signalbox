//! Same-filesystem atomic immutable-blob publication.
//!
//! The normative specification is `docs/spec/blob-storage.md`.

use std::{
    ffi::{OsStr, OsString},
    fmt::Write as _,
    fs,
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(target_os = "linux")]
use std::{
    collections::BTreeSet,
    ffi::CString,
    io::{Seek as _, SeekFrom},
    mem::MaybeUninit,
    os::unix::ffi::OsStringExt,
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use rustix::fs::{
    AtFlags, Mode, OFlags, RenameFlags, fchmod, open, openat, renameat, renameat_with, unlinkat,
};
#[cfg(target_os = "linux")]
use rustix::fs::{FlockOperation, RawDir, ResolveFlags, chmodat, flock, mkdirat, openat2};
#[cfg(unix)]
use rustix::process::geteuid;
use sha2::{Digest, Sha256};
use signalbox_blob_store::{
    BlobObjectKey, BlobPutOutcome, BlobReader, BlobStore, BlobStoreError, BlobStoreFuture,
    BlobVerificationFailure, ExpectedBlob, MAX_BLOB_RANGE_BYTES, OpenedBlob,
};
use signalbox_domain::BlobDigest;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

const STREAM_BUFFER_BYTES: usize = 64 * 1024;
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const PUBLICATION_DIRECTORY: &str = ".publish-v1";
const UPLOADS_DIRECTORY: &str = "uploads-v1";
const NAMESPACE_MARKER: &str = ".signalbox-blob-namespace-v1";
const MAX_NAMESPACE_MARKER_BYTES: u64 = 128;
const TEMPORARY_NAME_ATTEMPTS: usize = 16;
const MAX_BACKING_DEVICE_NODES: usize = 64;
const MAX_MOUNTINFO_BYTES: u64 = 1_048_576;

struct TemporaryBlobFile {
    directory: Arc<fs::File>,
    name: OsString,
    linked: bool,
}

#[cfg(target_os = "linux")]
struct PublicationDirectoryLock {
    descriptor: fs::File,
}

#[cfg(target_os = "linux")]
impl Drop for PublicationDirectoryLock {
    fn drop(&mut self) {
        let _ = flock(&self.descriptor, FlockOperation::Unlock);
    }
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
        if !self.linked {
            return;
        }
        self.linked = false;
        let _ = unlinkat(&self.directory, &self.name, AtFlags::empty());
    }
}

/// Filesystem store rooted at one deployment-owned storage namespace.
#[derive(Clone)]
pub struct FilesystemBlobStore {
    root: Arc<fs::File>,
    publication_directory: Arc<fs::File>,
}

/// Whether startup already has a durable binding for one configured namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceBindingState {
    /// The database binding exists, so the backend marker must already exist.
    Recorded,
    /// No database binding exists, so startup may create the marker once.
    New,
}

/// Descriptor-authenticated identity of one opened filesystem namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemNamespaceIdentity {
    canonical_path: PathBuf,
    device: u64,
    inode: u64,
    physical_path: PathBuf,
}

/// Validated filesystem root retained unopened-for-mutation until namespaces are compared.
pub struct OpenedFilesystemBlobRoot {
    configured_path: PathBuf,
    descriptor: fs::File,
    identity: FilesystemNamespaceIdentity,
}

impl std::fmt::Debug for OpenedFilesystemBlobRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OpenedFilesystemBlobRoot { root: <redacted> }")
    }
}

impl OpenedFilesystemBlobRoot {
    /// Opens and authenticates one root without creating, changing, or sweeping children.
    pub fn open(root: PathBuf) -> Result<Self, FilesystemBlobStoreConstructionError> {
        Self::open_with_locality_policy(root, true)
    }

    /// Opens a fixture root without host-locality classification.
    #[cfg(feature = "test-support")]
    pub fn open_without_locality_check_for_test(
        root: PathBuf,
    ) -> Result<Self, FilesystemBlobStoreConstructionError> {
        Self::open_with_locality_policy(root, false)
    }

    fn open_with_locality_policy(
        root: PathBuf,
        require_local_backing: bool,
    ) -> Result<Self, FilesystemBlobStoreConstructionError> {
        let (descriptor, identity) = open_validated_root(&root, require_local_backing)?;
        Ok(Self {
            configured_path: root,
            descriptor,
            identity,
        })
    }

    /// Returns the descriptor-authenticated namespace identity.
    pub const fn identity(&self) -> &FilesystemNamespaceIdentity {
        &self.identity
    }
}

/// Private crash-recovered staging namespace for connection-local uploads.
pub struct FilesystemBlobStaging {
    uploads_directory: Arc<fs::File>,
    identity: FilesystemNamespaceIdentity,
    sweep_on_drop: AtomicBool,
}

impl std::fmt::Debug for FilesystemBlobStaging {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FilesystemBlobStaging { root: <redacted> }")
    }
}

impl FilesystemBlobStaging {
    /// Opens the configured staging root and removes proven crash leftovers.
    pub fn try_new(root: PathBuf) -> Result<Self, FilesystemBlobStoreConstructionError> {
        Self::try_new_with_locality_policy(root, true)
    }

    /// Opens a fixture staging namespace without host-locality classification.
    #[cfg(feature = "test-support")]
    pub fn try_new_without_locality_check_for_test(
        root: PathBuf,
    ) -> Result<Self, FilesystemBlobStoreConstructionError> {
        Self::try_new_with_locality_policy(root, false)
    }

    fn try_new_with_locality_policy(
        root: PathBuf,
        require_local_backing: bool,
    ) -> Result<Self, FilesystemBlobStoreConstructionError> {
        let opened =
            OpenedFilesystemBlobRoot::open_with_locality_policy(root, require_local_backing)?;
        Self::from_opened(opened)
    }

    /// Prepares and sweeps a root only after its identity has been compared.
    pub fn from_opened(
        opened: OpenedFilesystemBlobRoot,
    ) -> Result<Self, FilesystemBlobStoreConstructionError> {
        let uploads_directory =
            prepare_staging_directory(&opened.descriptor).map_err(|source| {
                FilesystemBlobStoreConstructionError::PrepareStagingDirectory {
                    root: opened.configured_path,
                    source,
                }
            })?;
        Ok(Self {
            uploads_directory: Arc::new(uploads_directory),
            identity: opened.identity,
            sweep_on_drop: AtomicBool::new(true),
        })
    }

    /// Returns the descriptor-authenticated staging namespace identity.
    pub const fn identity(&self) -> &FilesystemNamespaceIdentity {
        &self.identity
    }

    /// Removes every proven regular upload spool before clean shutdown.
    pub fn sweep(&self) -> io::Result<()> {
        sweep_private_temporary_directory(&self.uploads_directory)?;
        self.uploads_directory.sync_all()
    }

    /// Creates one private connection-local upload spool.
    pub async fn create_upload(&self) -> io::Result<FilesystemBlobUpload> {
        let directory = self.uploads_directory.clone();
        let (temporary, file) =
            tokio::task::spawn_blocking(move || create_temporary_blob_file(directory))
                .await
                .map_err(io::Error::other)??;
        Ok(FilesystemBlobUpload {
            temporary,
            file: tokio::fs::File::from_std(file),
        })
    }

    /// Prevents cleanup after the daemon has lost its exclusive staging guard.
    pub fn disarm_sweep_on_drop(&self) {
        self.sweep_on_drop.store(false, Ordering::Release);
    }
}

impl Drop for FilesystemBlobStaging {
    fn drop(&mut self) {
        if !self.sweep_on_drop.load(Ordering::Acquire) {
            return;
        }
        let _ = sweep_private_temporary_directory(&self.uploads_directory);
        let _ = self.uploads_directory.sync_all();
    }
}

/// One create-new private upload spool, removed when its connection state drops.
pub struct FilesystemBlobUpload {
    temporary: TemporaryBlobFile,
    file: tokio::fs::File,
}

impl std::fmt::Debug for FilesystemBlobUpload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FilesystemBlobUpload { path: <redacted> }")
    }
}

impl FilesystemBlobUpload {
    /// Appends one already-bounded chunk in exact physical order.
    pub async fn append(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.file.write_all(chunk).await
    }

    /// Flushes the spool and returns its descriptor at offset zero as a stream.
    pub async fn into_reader(mut self) -> io::Result<BlobReader> {
        self.file.flush().await?;
        self.file.seek(std::io::SeekFrom::Start(0)).await?;
        self.temporary.remove()?;
        Ok(Box::new(self.file))
    }
}

impl FilesystemNamespaceIdentity {
    /// Returns the canonical path resolving to the opened directory generation.
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// Returns the opened directory's device and inode identity.
    pub const fn device_inode(&self) -> (u64, u64) {
        (self.device, self.inode)
    }

    /// Returns the path within the underlying mounted filesystem, resolving
    /// bind-mount aliases to their shared physical ancestry.
    pub fn physical_path(&self) -> &Path {
        &self.physical_path
    }
}

impl std::fmt::Debug for FilesystemBlobStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FilesystemBlobStore { root: <redacted> }")
    }
}

impl FilesystemBlobStore {
    /// Constructs a store at an absolute existing directory.
    pub fn try_new(root: PathBuf) -> Result<Self, FilesystemBlobStoreConstructionError> {
        Self::try_new_with_locality_policy(root, true)
    }

    /// Opens one configured store and establishes or verifies its namespace marker.
    pub fn try_new_bound(
        root: PathBuf,
        namespace_id: Uuid,
        binding_state: NamespaceBindingState,
    ) -> Result<(Self, FilesystemNamespaceIdentity), FilesystemBlobStoreConstructionError> {
        Self::from_opened_bound(
            OpenedFilesystemBlobRoot::open(root)?,
            namespace_id,
            binding_state,
        )
    }

    /// Establishes a namespace marker and publication area after identity comparison.
    pub fn from_opened_bound(
        opened: OpenedFilesystemBlobRoot,
        namespace_id: Uuid,
        binding_state: NamespaceBindingState,
    ) -> Result<(Self, FilesystemNamespaceIdentity), FilesystemBlobStoreConstructionError> {
        Self::from_opened_bound_inner(opened, namespace_id, binding_state)
    }

    /// Opens a conformance namespace while retaining every check except host
    /// backing-device locality, which shared CI cannot establish.
    #[cfg(feature = "test-support")]
    pub fn try_new_bound_for_conformance(
        root: PathBuf,
        namespace_id: Uuid,
        binding_state: NamespaceBindingState,
    ) -> Result<(Self, FilesystemNamespaceIdentity), FilesystemBlobStoreConstructionError> {
        Self::from_opened_bound(
            OpenedFilesystemBlobRoot::open_without_locality_check_for_test(root)?,
            namespace_id,
            binding_state,
        )
    }

    /// Constructs a conformance fixture while retaining every check except
    /// host backing-device locality, which shared CI cannot establish.
    #[cfg(feature = "test-support")]
    pub fn try_new_for_conformance(
        root: PathBuf,
    ) -> Result<Self, FilesystemBlobStoreConstructionError> {
        Self::try_new_with_locality_policy(root, false)
    }

    fn try_new_with_locality_policy(
        root: PathBuf,
        require_local_backing: bool,
    ) -> Result<Self, FilesystemBlobStoreConstructionError> {
        let (root_descriptor, _) = open_validated_root(&root, require_local_backing)?;
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

    fn from_opened_bound_inner(
        opened: OpenedFilesystemBlobRoot,
        namespace_id: Uuid,
        binding_state: NamespaceBindingState,
    ) -> Result<(Self, FilesystemNamespaceIdentity), FilesystemBlobStoreConstructionError> {
        initialize_namespace_marker(&opened.descriptor, namespace_id, binding_state).map_err(
            |source| FilesystemBlobStoreConstructionError::PrepareNamespaceMarker {
                root: opened.configured_path.clone(),
                source,
            },
        )?;
        let root_descriptor = Arc::new(opened.descriptor);
        let publication_directory = Arc::new(
            prepare_publication_directory(&root_descriptor).map_err(|source| {
                FilesystemBlobStoreConstructionError::PreparePublicationDirectory {
                    root: opened.configured_path,
                    source,
                }
            })?,
        );
        Ok((
            Self {
                root: root_descriptor,
                publication_directory,
            },
            opened.identity,
        ))
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
                        verify_file_at_key(
                            self.root.clone(),
                            key.clone(),
                            expected,
                            "verify recorded object key",
                        )
                        .await?;
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

        #[cfg(target_os = "linux")]
        let _publication_lock = acquire_publication_lock(
            self.publication_directory.clone(),
            FlockOperation::LockShared,
        )
        .await?;
        #[cfg(not(target_os = "linux"))]
        let _publication_lock =
            acquire_publication_lock(self.publication_directory.clone(), ()).await?;
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
                self.root.clone(),
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
                verify_file_at_key(
                    self.root.clone(),
                    key.clone(),
                    expected,
                    "verify recorded object key",
                )
                .await?;
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
                        verify_file_at_key(
                            self.root.clone(),
                            key.clone(),
                            expected,
                            "verify recorded object key",
                        )
                        .await?;
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
                            self.root.clone(),
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

    async fn open_verified_inner(
        &self,
        expected: ExpectedBlob,
        key: &BlobObjectKey,
    ) -> Result<OpenedBlob, BlobStoreError> {
        if !filesystem_key_is_admitted(key) {
            return Err(BlobStoreError::unavailable("reject reserved object key"));
        }
        let (mut file, byte_length) =
            open_private_regular_file(self.root.clone(), key.clone(), "open verified object")
                .await?;
        #[cfg(target_os = "linux")]
        let _publication_lock = acquire_publication_lock(
            self.publication_directory.clone(),
            FlockOperation::LockShared,
        )
        .await?;
        #[cfg(not(target_os = "linux"))]
        let _publication_lock =
            acquire_publication_lock(self.publication_directory.clone(), ()).await?;
        let publication_directory = self.publication_directory.clone();
        let (temporary, pinned_file) =
            tokio::task::spawn_blocking(move || create_temporary_blob_file(publication_directory))
                .await
                .map_err(|source| BlobStoreError::io("join verified spool creation", source))?
                .map_err(|source| BlobStoreError::io("create verified spool", source))?;
        let mut pinned_file = tokio::fs::File::from_std(pinned_file);
        verify_opened_file_into(
            &mut file,
            &mut pinned_file,
            expected,
            "verify opened object",
        )
        .await?;
        pinned_file
            .flush()
            .await
            .map_err(|source| BlobStoreError::io("flush verified spool", source))?;
        pinned_file
            .seek(std::io::SeekFrom::Start(0))
            .await
            .map_err(|source| BlobStoreError::io("rewind verified spool", source))?;
        remove_temporary_file(temporary).await?;
        Ok(OpenedBlob::new(byte_length, Box::new(pinned_file)))
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
        ensure_destination_parent_from_root(self.root.clone(), key).await
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

    fn open_verified<'a>(
        &'a self,
        expected: ExpectedBlob,
        key: &'a BlobObjectKey,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(self.open_verified_inner(expected, key))
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
    root: Arc<fs::File>,
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
    verify_file_at_key(root, key.clone(), expected, "verify recorded object key").await?;
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

async fn verify_file_at_key(
    root: Arc<fs::File>,
    key: BlobObjectKey,
    expected: ExpectedBlob,
    operation: &'static str,
) -> Result<(), BlobStoreError> {
    let reachable_parent = ensure_destination_parent_from_root(root.clone(), &key).await?;
    sync_open_directory(reachable_parent).await?;
    let (mut file, _) = open_private_regular_file(root, key, operation).await?;
    verify_opened_file(&mut file, expected, operation).await
}

async fn ensure_destination_parent_from_root(
    root: Arc<fs::File>,
    key: &BlobObjectKey,
) -> Result<Arc<fs::File>, BlobStoreError> {
    let relative_parent = std::path::Path::new(key.as_str())
        .parent()
        .ok_or_else(|| BlobStoreError::unavailable("derive destination parent"))?;
    let mut current = root;
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

async fn verify_opened_file(
    file: &mut tokio::fs::File,
    expected: ExpectedBlob,
    operation: &'static str,
) -> Result<(), BlobStoreError> {
    verify_opened_file_with_destination(file, None, expected, operation).await
}

async fn verify_opened_file_into(
    file: &mut tokio::fs::File,
    destination: &mut tokio::fs::File,
    expected: ExpectedBlob,
    operation: &'static str,
) -> Result<(), BlobStoreError> {
    verify_opened_file_with_destination(file, Some(destination), expected, operation).await
}

async fn verify_opened_file_with_destination(
    file: &mut tokio::fs::File,
    mut destination: Option<&mut tokio::fs::File>,
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
        if let Some(destination) = destination.as_deref_mut() {
            destination
                .write_all(&buffer[..read])
                .await
                .map_err(|source| BlobStoreError::io("write verified spool", source))?;
        }
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

fn open_validated_root(
    root: &Path,
    require_local_backing: bool,
) -> Result<(fs::File, FilesystemNamespaceIdentity), FilesystemBlobStoreConstructionError> {
    if !root.is_absolute() {
        return Err(FilesystemBlobStoreConstructionError::NotAbsolute {
            root: root.to_path_buf(),
        });
    }
    let inspect = |source| FilesystemBlobStoreConstructionError::Inspect {
        root: root.to_path_buf(),
        source,
    };
    let root_descriptor = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(io::Error::from)
    .map_err(inspect)?;
    let metadata = root_descriptor.metadata().map_err(inspect)?;
    if !metadata.is_dir() {
        return Err(FilesystemBlobStoreConstructionError::NotDirectory {
            root: root.to_path_buf(),
        });
    }
    if !private_directory_metadata(&metadata) {
        return Err(FilesystemBlobStoreConstructionError::NotPrivate {
            root: root.to_path_buf(),
        });
    }
    let positively_local = !require_local_backing
        || positively_classified_local_filesystem(&root_descriptor).map_err(inspect)?;
    if !positively_local {
        return Err(
            FilesystemBlobStoreConstructionError::UnclassifiedFilesystem {
                root: root.to_path_buf(),
            },
        );
    }
    let canonical_path = fs::canonicalize(root).map_err(inspect)?;
    let canonical_descriptor = open(
        &canonical_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(io::Error::from)
    .map_err(inspect)?;
    let canonical_metadata = canonical_descriptor.metadata().map_err(inspect)?;
    let (device, inode) = metadata_device_inode(&metadata);
    let mount_id = descriptor_mount_id(&root_descriptor).map_err(inspect)?;
    let physical_path = physical_namespace_path(&canonical_path, mount_id).map_err(inspect)?;
    if metadata_device_inode(&canonical_metadata) != (device, inode) {
        return Err(FilesystemBlobStoreConstructionError::UnstableIdentity {
            root: root.to_path_buf(),
        });
    }
    Ok((
        root_descriptor,
        FilesystemNamespaceIdentity {
            canonical_path,
            device,
            inode,
            physical_path,
        },
    ))
}

#[cfg(target_os = "linux")]
fn descriptor_mount_id(descriptor: &fs::File) -> io::Result<u64> {
    let facts = rustix::fs::statx(
        descriptor,
        "",
        AtFlags::EMPTY_PATH,
        rustix::fs::StatxFlags::MNT_ID,
    )
    .map_err(io::Error::from)?;
    if facts.stx_mask & rustix::fs::StatxFlags::MNT_ID.bits() != 0 {
        Ok(facts.stx_mnt_id)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem mount identity is unavailable",
        ))
    }
}

#[cfg(target_os = "linux")]
fn physical_namespace_path(canonical_path: &Path, mount_id: u64) -> io::Result<PathBuf> {
    let mut mountinfo = fs::File::open("/proc/self/mountinfo")?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut mountinfo)
        .take(MAX_MOUNTINFO_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let byte_length = u64::try_from(bytes.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "mount inventory byte count is unrepresentable",
        )
    })?;
    if byte_length > MAX_MOUNTINFO_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mount inventory exceeds its startup bound",
        ));
    }
    for line in bytes.split(|byte| *byte == b'\n') {
        let fields = line
            .split(|byte| byte.is_ascii_whitespace())
            .collect::<Vec<_>>();
        if fields.len() < 6 || parse_decimal_u64(fields[0]) != Some(mount_id) {
            continue;
        }
        let root = PathBuf::from(OsString::from_vec(decode_mountinfo_path(fields[3])?));
        let mount_point = PathBuf::from(OsString::from_vec(decode_mountinfo_path(fields[4])?));
        let relative = canonical_path.strip_prefix(&mount_point).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "opened namespace is outside its reported mount point",
            )
        })?;
        return Ok(root.join(relative));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "opened namespace mount is absent from the process mount inventory",
    ))
}

#[cfg(target_os = "linux")]
fn parse_decimal_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0_u64, |value, byte| {
        let digit = byte.checked_sub(b'0')?;
        if digit > 9 {
            return None;
        }
        value.checked_mul(10)?.checked_add(u64::from(digit))
    })
}

#[cfg(target_os = "linux")]
fn decode_mountinfo_path(encoded: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        if encoded[index] != b'\\' {
            decoded.push(encoded[index]);
            index += 1;
            continue;
        }
        let Some(octal) = encoded.get(index + 1..index + 4) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "mount inventory contains a truncated path escape",
            ));
        };
        let value = octal.iter().try_fold(0_u8, |value, digit| {
            value
                .checked_mul(8)?
                .checked_add(digit.checked_sub(b'0').filter(|digit| *digit < 8)?)
        });
        let Some(value) = value else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "mount inventory contains an invalid path escape",
            ));
        };
        decoded.push(value);
        index += 4;
    }
    Ok(decoded)
}

#[cfg(not(target_os = "linux"))]
fn descriptor_mount_id(_descriptor: &fs::File) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem mount identity requires Linux statx",
    ))
}

#[cfg(not(target_os = "linux"))]
fn physical_namespace_path(_canonical_path: &Path, _mount_id: u64) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem mount ancestry requires Linux mount inventory",
    ))
}

#[cfg(unix)]
fn metadata_device_inode(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn metadata_device_inode(_metadata: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn initialize_namespace_marker(
    root: &fs::File,
    namespace_id: Uuid,
    binding_state: NamespaceBindingState,
) -> io::Result<()> {
    let expected = format!("{}\n", namespace_id.hyphenated());
    if binding_state == NamespaceBindingState::New {
        match create_namespace_marker(root, expected.as_bytes()) {
            Ok(()) => return Ok(()),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(source),
        }
    }
    verify_namespace_marker(root, expected.as_bytes())
}

fn create_namespace_marker(root: &fs::File, expected: &[u8]) -> io::Result<()> {
    let (temporary, mut marker) = create_temporary_blob_file(Arc::new(root.try_clone()?))?;
    fchmod(&marker, Mode::RUSR | Mode::WUSR).map_err(io::Error::from)?;
    if !private_regular_file_metadata(&marker.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "temporary namespace marker is not a private regular file",
        ));
    }
    marker.write_all(expected)?;
    marker.sync_all()?;
    match temporary.publish_noclobber(root, OsStr::new(NAMESPACE_MARKER))? {
        None => root.sync_all(),
        Some(temporary) => {
            temporary.remove()?;
            root.sync_all()?;
            Err(io::Error::from(io::ErrorKind::AlreadyExists))
        }
    }
}

fn verify_namespace_marker(root: &fs::File, expected: &[u8]) -> io::Result<()> {
    let mut marker = open_relative_regular_name(root, OsStr::new(NAMESPACE_MARKER))?;
    if !private_regular_file_metadata(&marker.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "namespace marker is not a private regular file",
        ));
    }
    let mut bytes = Vec::new();
    std::io::Read::take(&mut marker, MAX_NAMESPACE_MARKER_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "namespace marker disagrees with the configured identity",
        ));
    }
    Ok(())
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

#[cfg(target_os = "linux")]
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

#[cfg(not(target_os = "linux"))]
fn random_temporary_name() -> io::Result<OsString> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem blob staging requires Linux descriptor-relative directories",
    ))
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

#[cfg(target_os = "linux")]
fn lock_publication_directory(
    directory: &fs::File,
    operation: FlockOperation,
) -> io::Result<PublicationDirectoryLock> {
    let descriptor = fs::File::from(
        openat2(
            directory,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(io::Error::from)?,
    );
    flock(&descriptor, operation).map_err(io::Error::from)?;
    Ok(PublicationDirectoryLock { descriptor })
}

#[cfg(target_os = "linux")]
async fn acquire_publication_lock(
    directory: Arc<fs::File>,
    operation: FlockOperation,
) -> Result<PublicationDirectoryLock, BlobStoreError> {
    tokio::task::spawn_blocking(move || lock_publication_directory(&directory, operation))
        .await
        .map_err(|source| BlobStoreError::io("join publication lock acquisition", source))?
        .map_err(|source| BlobStoreError::io("acquire publication lock", source))
}

#[cfg(not(target_os = "linux"))]
async fn acquire_publication_lock(
    _directory: Arc<fs::File>,
    _operation: (),
) -> Result<(), BlobStoreError> {
    Err(BlobStoreError::io(
        "acquire publication lock",
        io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem blob publication locking requires Linux",
        ),
    ))
}

#[cfg(target_os = "linux")]
fn prepare_staging_directory(root: &fs::File) -> io::Result<fs::File> {
    let created = match mkdirat(
        root,
        UPLOADS_DIRECTORY,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    ) {
        Ok(()) => true,
        Err(source) if source == rustix::io::Errno::EXIST => false,
        Err(source) => return Err(io::Error::from(source)),
    };
    if created {
        chmodat(
            root,
            UPLOADS_DIRECTORY,
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
            AtFlags::empty(),
        )
        .map_err(io::Error::from)?;
    }
    let uploads = fs::File::from(
        openat2(
            root,
            UPLOADS_DIRECTORY,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(io::Error::from)?,
    );
    if !private_directory_metadata(&uploads.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "blob upload directory is not private",
        ));
    }
    root.sync_all()?;
    sweep_private_temporary_directory(&uploads)?;
    uploads.sync_all()?;
    Ok(uploads)
}

#[cfg(not(target_os = "linux"))]
fn prepare_staging_directory(_root: &fs::File) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem blob staging requires Linux descriptor-relative directories",
    ))
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
    let _publication_lock = lock_publication_directory(directory, FlockOperation::LockExclusive)?;
    sweep_private_temporary_directory(directory)
}

#[cfg(target_os = "linux")]
fn sweep_private_temporary_directory(directory: &fs::File) -> io::Result<()> {
    let mut directory_offset = directory;
    directory_offset.seek(SeekFrom::Start(0))?;
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
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
            )
            .map_err(io::Error::from)?,
        );
        if !recoverable_temporary_file_metadata(&file.metadata()?) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "publication directory contains an unowned or non-regular entry",
            ));
        }
        unlinkat(directory, &name, AtFlags::empty()).map_err(io::Error::from)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn sweep_private_temporary_directory(_directory: &fs::File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem blob staging requires Linux descriptor-relative directories",
    ))
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

#[cfg(unix)]
fn recoverable_temporary_file_metadata(metadata: &fs::Metadata) -> bool {
    let permission_bits = metadata.mode() & PERMISSION_MASK;
    metadata.is_file()
        && metadata.uid() == geteuid().as_raw()
        && permission_bits & !FILE_MODE == 0
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
    if !matches!(
        filesystem,
        EXT_SUPER_MAGIC | XFS_SUPER_MAGIC | BTRFS_SUPER_MAGIC | ZFS_SUPER_MAGIC | F2FS_SUPER_MAGIC
    ) {
        return Ok(false);
    }
    positively_classified_local_backing_devices(root.metadata()?.dev())
}

#[cfg(target_os = "linux")]
fn positively_classified_local_backing_devices(device: u64) -> io::Result<bool> {
    let major = rustix::fs::major(device);
    let minor = rustix::fs::minor(device);
    let mut pending = vec![PathBuf::from(format!("/sys/dev/block/{major}:{minor}"))];
    let mut visited = BTreeSet::new();
    while let Some(device_path) = pending.pop() {
        let canonical = fs::canonicalize(device_path)?;
        if !visited.insert(canonical.clone()) {
            continue;
        }
        if visited.len() > MAX_BACKING_DEVICE_NODES {
            return Ok(false);
        }
        let slaves_path = canonical.join("slaves");
        let slaves = match fs::read_dir(slaves_path) {
            Ok(slaves) => Some(slaves),
            Err(source) if source.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(source),
        };
        let mut has_slave = false;
        if let Some(slaves) = slaves {
            for slave in slaves {
                has_slave = true;
                pending.push(slave?.path());
                if pending.len() + visited.len() > MAX_BACKING_DEVICE_NODES {
                    return Ok(false);
                }
            }
        }
        if !has_slave && !local_block_transport_leaf(&canonical) {
            return Ok(false);
        }
    }
    Ok(!visited.is_empty())
}

#[cfg(target_os = "linux")]
fn local_block_transport_leaf(path: &Path) -> bool {
    if !path.starts_with("/sys/devices") || path.starts_with("/sys/devices/virtual") {
        return false;
    }
    if path.components().any(|component| {
        let std::path::Component::Normal(component) = component else {
            return false;
        };
        component.as_encoded_bytes().starts_with(b"vhci_hcd")
    }) {
        return false;
    }
    path.components().any(|component| {
        let std::path::Component::Normal(component) = component else {
            return false;
        };
        let component = component.as_encoded_bytes();
        component.starts_with(b"ata")
            || component.starts_with(b"mmc")
            || component.starts_with(b"nvme")
            || component.starts_with(b"usb")
    })
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
    /// The configured path changed generations while startup authenticated it.
    UnstableIdentity { root: PathBuf },
    /// The backend namespace marker could not be established or authenticated.
    PrepareNamespaceMarker { root: PathBuf, source: io::Error },
    /// The private crash-recovery upload directory could not be prepared.
    PrepareStagingDirectory { root: PathBuf, source: io::Error },
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
            Self::UnstableIdentity { .. } => {
                "FilesystemBlobStoreConstructionError::UnstableIdentity"
            }
            Self::PrepareNamespaceMarker { .. } => {
                "FilesystemBlobStoreConstructionError::PrepareNamespaceMarker"
            }
            Self::PrepareStagingDirectory { .. } => {
                "FilesystemBlobStoreConstructionError::PrepareStagingDirectory"
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
            Self::UnstableIdentity { .. } => {
                formatter.write_str("filesystem blob-store root identity changed during startup")
            }
            Self::PrepareNamespaceMarker { .. } => {
                formatter.write_str("filesystem blob-store namespace marker cannot be prepared")
            }
            Self::PrepareStagingDirectory { .. } => {
                formatter.write_str("filesystem blob staging directory cannot be prepared")
            }
            Self::PreparePublicationDirectory { .. } => formatter
                .write_str("filesystem blob-store publication directory cannot be prepared"),
        }
    }
}

impl std::error::Error for FilesystemBlobStoreConstructionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Inspect { source, .. }
            | Self::PrepareNamespaceMarker { source, .. }
            | Self::PrepareStagingDirectory { source, .. }
            | Self::PreparePublicationDirectory { source, .. } => Some(source),
            Self::NotAbsolute { .. }
            | Self::NotDirectory { .. }
            | Self::NotPrivate { .. }
            | Self::UnclassifiedFilesystem { .. }
            | Self::UnstableIdentity { .. } => None,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    const PRIMARY_NAMESPACE: u128 = 0x5a10_0001;
    const SECONDARY_NAMESPACE: u128 = 0x5a10_0002;

    fn open_bound_fixture(
        root: &Path,
        namespace: u128,
        state: NamespaceBindingState,
    ) -> Result<
        (FilesystemBlobStore, FilesystemNamespaceIdentity),
        FilesystemBlobStoreConstructionError,
    > {
        let opened =
            OpenedFilesystemBlobRoot::open_with_locality_policy(root.to_path_buf(), false)?;
        FilesystemBlobStore::from_opened_bound(opened, Uuid::from_u128(namespace), state)
    }

    /// descriptor-authenticated root inspection is mutation-free so
    /// registry overlap checks always precede marker creation and recovery sweeps.
    #[test]
    fn opened_root_defers_every_namespace_mutation() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");
        let publication = root.path().join(PUBLICATION_DIRECTORY);
        fs::create_dir(&publication).expect("the fixture publication directory is created");
        fs::set_permissions(&publication, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture publication directory is private");
        let sentinel = publication.join("pre-validation-sentinel");
        let sentinel_content = b"must remain before validation";
        fs::write(&sentinel, sentinel_content).expect("the sentinel is written");
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(FILE_MODE))
            .expect("the sentinel is private");

        let opened =
            OpenedFilesystemBlobRoot::open_with_locality_policy(root.path().to_path_buf(), false)
                .expect("the private root can be inspected");

        assert_eq!(opened.identity().canonical_path(), root.path());
        assert_eq!(
            fs::read(&sentinel).expect("inspection leaves the sentinel intact"),
            sentinel_content
        );
        assert!(!root.path().join(NAMESPACE_MARKER).exists());
    }

    #[test]
    fn new_namespace_marker_is_durable_and_reopens_as_recorded() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");

        let (_, created_identity) =
            open_bound_fixture(root.path(), PRIMARY_NAMESPACE, NamespaceBindingState::New)
                .expect("a new namespace marker is established");
        let marker =
            fs::read(root.path().join(NAMESPACE_MARKER)).expect("the namespace marker is readable");
        let (_, recorded_identity) = open_bound_fixture(
            root.path(),
            PRIMARY_NAMESPACE,
            NamespaceBindingState::Recorded,
        )
        .expect("the exact recorded namespace reopens");
        let expected_marker = format!("{}\n", Uuid::from_u128(PRIMARY_NAMESPACE));

        assert_eq!(marker, expected_marker.as_bytes());
        assert_eq!(recorded_identity, created_identity);
    }

    #[test]
    fn namespace_initialization_leaves_another_attempts_temporary_file_untouched() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");
        let temporary = root.path().join(".tmp-concurrent-marker-attempt");
        let concurrent_content = b"another initializer still owns this file";
        fs::write(&temporary, concurrent_content)
            .expect("the fixture writes another attempt's temporary marker");
        fs::set_permissions(&temporary, fs::Permissions::from_mode(FILE_MODE))
            .expect("the other attempt's marker is private");

        open_bound_fixture(root.path(), PRIMARY_NAMESPACE, NamespaceBindingState::New)
            .expect("new startup publishes through its own temporary marker");
        let marker =
            fs::read(root.path().join(NAMESPACE_MARKER)).expect("the final marker is readable");
        let expected_marker = format!("{}\n", Uuid::from_u128(PRIMARY_NAMESPACE));

        assert_eq!(marker, expected_marker.as_bytes());
        assert_eq!(
            fs::read(temporary).expect("the other attempt's marker remains readable"),
            concurrent_content
        );
    }

    #[test]
    fn recorded_namespace_requires_an_existing_marker() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");

        let error = open_bound_fixture(
            root.path(),
            PRIMARY_NAMESPACE,
            NamespaceBindingState::Recorded,
        )
        .expect_err("a recorded namespace cannot create its missing marker");

        assert!(matches!(
            error,
            FilesystemBlobStoreConstructionError::PrepareNamespaceMarker { .. }
        ));
        assert!(!root.path().join(NAMESPACE_MARKER).exists());
    }

    #[test]
    fn namespace_marker_rejects_another_configured_identity() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");
        open_bound_fixture(root.path(), PRIMARY_NAMESPACE, NamespaceBindingState::New)
            .expect("the first namespace marker is established");

        let error =
            open_bound_fixture(root.path(), SECONDARY_NAMESPACE, NamespaceBindingState::New)
                .expect_err("an existing marker cannot be clobbered");

        assert!(matches!(
            error,
            FilesystemBlobStoreConstructionError::PrepareNamespaceMarker { .. }
        ));
    }

    #[test]
    fn staging_startup_sweeps_proven_regular_spools() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");
        let uploads = root.path().join(UPLOADS_DIRECTORY);
        fs::create_dir(&uploads).expect("the fixture creates the upload directory");
        fs::set_permissions(&uploads, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the upload directory is private");
        let spool = uploads.join("orphan");
        fs::write(&spool, b"partial upload").expect("the fixture creates an orphan spool");
        fs::set_permissions(&spool, fs::Permissions::from_mode(FILE_MODE))
            .expect("the orphan spool is private");

        let staging =
            FilesystemBlobStaging::try_new_with_locality_policy(root.path().to_path_buf(), false)
                .expect("the staging namespace opens");

        assert!(!spool.exists());
        assert_eq!(staging.identity().canonical_path(), root.path());
    }

    #[test]
    fn staging_drop_sweeps_proven_regular_spools() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");
        let staging =
            FilesystemBlobStaging::try_new_with_locality_policy(root.path().to_path_buf(), false)
                .expect("the staging namespace opens");
        let spool = root.path().join(UPLOADS_DIRECTORY).join("active-upload");
        fs::write(&spool, b"partial upload").expect("the fixture creates an active spool");
        fs::set_permissions(&spool, fs::Permissions::from_mode(FILE_MODE))
            .expect("the active spool is private");

        drop(staging);

        assert!(!spool.exists());
    }

    /// an active upload retains only one private linked spool, then
    /// hands its descriptor to publication after unlinking the staging name.
    #[tokio::test]
    async fn upload_spool_streams_exact_bytes_after_unlink() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");
        let staging =
            FilesystemBlobStaging::try_new_with_locality_policy(root.path().to_path_buf(), false)
                .expect("the staging namespace opens");
        let mut upload = staging
            .create_upload()
            .await
            .expect("one private upload spool is created");
        upload
            .append(b"first")
            .await
            .expect("the first bounded chunk appends");
        upload
            .append(b"-second")
            .await
            .expect("the second bounded chunk appends");
        let linked_count = fs::read_dir(root.path().join(UPLOADS_DIRECTORY))
            .expect("the uploads directory is readable")
            .count();
        let mut reader = upload
            .into_reader()
            .await
            .expect("the unlinked descriptor becomes a reader");
        let unlinked_count = fs::read_dir(root.path().join(UPLOADS_DIRECTORY))
            .expect("the uploads directory remains readable")
            .count();
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .await
            .expect("the unlinked descriptor remains readable");

        assert_eq!(linked_count, 1);
        assert_eq!(unlinked_count, 0);
        assert_eq!(bytes, b"first-second");
    }

    /// abandoning one connection-local upload removes its linked
    /// private spool without needing a daemon-wide sweep.
    #[tokio::test]
    async fn dropped_upload_spool_is_removed() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");
        let staging =
            FilesystemBlobStaging::try_new_with_locality_policy(root.path().to_path_buf(), false)
                .expect("the staging namespace opens");
        let mut upload = staging
            .create_upload()
            .await
            .expect("one private upload spool is created");
        upload
            .append(b"partial")
            .await
            .expect("one bounded chunk appends");
        let linked_count = fs::read_dir(root.path().join(UPLOADS_DIRECTORY))
            .expect("the uploads directory is readable")
            .count();
        drop(upload);
        let dropped_count = fs::read_dir(root.path().join(UPLOADS_DIRECTORY))
            .expect("the uploads directory remains readable")
            .count();

        assert_eq!(linked_count, 1);
        assert_eq!(dropped_count, 0);
    }

    #[test]
    fn disarmed_staging_drop_leaves_spools_for_the_guard_holder() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");
        let staging =
            FilesystemBlobStaging::try_new_with_locality_policy(root.path().to_path_buf(), false)
                .expect("the staging namespace opens");
        let spool = root
            .path()
            .join(UPLOADS_DIRECTORY)
            .join("replacement-upload");
        let spool_content = b"owned by the replacement daemon";
        fs::write(&spool, spool_content).expect("the fixture creates a replacement spool");
        fs::set_permissions(&spool, fs::Permissions::from_mode(FILE_MODE))
            .expect("the replacement spool is private");

        staging.disarm_sweep_on_drop();
        drop(staging);

        assert_eq!(
            fs::read(spool).expect("disarmed cleanup leaves the replacement spool"),
            spool_content
        );
    }

    #[test]
    fn filesystem_rejects_mounted_child_directories() {
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

    #[tokio::test]
    async fn filesystem_rejects_an_unreachable_retained_parent_publication() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let content = b"descriptor-bound bytes";
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
        let root_descriptor = Arc::new(
            open(
                root.path(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map(fs::File::from)
            .expect("the root descriptor opens"),
        );
        let (temporary, mut output) = create_temporary_blob_file(publication)
            .expect("the fixture creates a descriptor-relative temporary file");
        output
            .write_all(content)
            .expect("the fixture writes the temporary bytes");
        output
            .sync_all()
            .expect("the fixture synchronizes the temporary bytes");
        fs::rename(&destination_path, &moved_destination)
            .expect("the destination generation is renamed");
        fs::create_dir(&destination_path).expect("a replacement destination path is created");
        fs::set_permissions(
            &destination_path,
            fs::Permissions::from_mode(DIRECTORY_MODE),
        )
        .expect("the replacement destination is private");

        let outcome = temporary
            .publish_noclobber(&destination, OsStr::new("object"))
            .expect("publication through the retained descriptor succeeds");
        let key = BlobObjectKey::try_from_recorded("destination/object")
            .expect("the fixture key is relative");
        let expected = ExpectedBlob::try_new(
            BlobDigest::digest(content),
            u64::try_from(content.len()).expect("the fixture length fits u64"),
        )
        .expect("the fixture is nonempty");

        let error = verify_file_at_key(
            root_descriptor,
            key,
            expected,
            "verify test recorded object key",
        )
        .await
        .expect_err("the replacement tree cannot resolve the published object");

        assert!(outcome.is_none());
        assert!(moved_destination.join("object").is_file());
        assert!(!destination_path.join("object").exists());
        assert_eq!(
            error.kind(),
            signalbox_blob_store::BlobStoreFailureKind::NotFound
        );
    }

    #[test]
    fn filesystem_accepts_only_explicitly_local_block_transport_leaves() {
        let local = Path::new("/sys/devices/pci0000:00/0000:00:01.0/nvme/nvme0/nvme0n1");
        let usb_ip = Path::new(
            "/sys/devices/platform/vhci_hcd.0/usb2/2-1/2-1:1.0/host7/target7:0:0/7:0:0:0/block/sdb",
        );
        let virtio = Path::new("/sys/devices/pci0000:00/0000:00:02.0/virtio1/block/vda");
        let network_block = Path::new("/sys/devices/virtual/block/nbd0");
        let iscsi = Path::new("/sys/devices/platform/host2/session1/target2:0:0/2:0:0:0/block/sdb");

        assert!(local_block_transport_leaf(local));
        assert!(!local_block_transport_leaf(usb_ip));
        assert!(!local_block_transport_leaf(virtio));
        assert!(!local_block_transport_leaf(network_block));
        assert!(!local_block_transport_leaf(iscsi));
    }

    #[test]
    fn filesystem_sweeps_an_interrupted_mode_zero_temporary_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        let temporary_path = root.path().join(".tmp-interrupted");
        fs::write(&temporary_path, b"unpublished bytes")
            .expect("the fixture writes an interrupted temporary file");
        fs::set_permissions(&temporary_path, fs::Permissions::from_mode(0o000))
            .expect("the fixture reproduces an umask-masked creation mode");
        let directory = fs::File::from(
            open(
                root.path(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("the publication directory opens"),
        );

        sweep_publication_directory(&directory)
            .expect("the interrupted private temporary file is recoverable");

        assert!(!temporary_path.exists());
    }

    /// cancelling a publication unlinks its temporary file before the
    /// publication lock can leave scope.
    #[test]
    fn temporary_publication_drop_unlinks_before_returning() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        let directory = Arc::new(fs::File::from(
            open(
                root.path(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("the temporary directory opens"),
        ));
        let (temporary, output) = create_temporary_blob_file(directory)
            .expect("the fixture creates a publication temporary file");
        let temporary_path = root.path().join(&temporary.name);
        assert!(temporary_path.exists());

        drop(output);
        drop(temporary);

        assert!(!temporary_path.exists());
    }

    /// recovery sweeps for one publication namespace serialize even
    /// when a replacement process begins opening the same bound store.
    #[test]
    fn publication_recovery_waits_for_the_namespace_lock() {
        let root = tempfile::TempDir::new().expect("the fixture creates a temporary root");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the fixture root is private");
        let publication_path = root.path().join(PUBLICATION_DIRECTORY);
        fs::create_dir(&publication_path).expect("the publication directory is created");
        fs::set_permissions(
            &publication_path,
            fs::Permissions::from_mode(DIRECTORY_MODE),
        )
        .expect("the publication directory is private");
        let publication = fs::File::from(
            open(
                &publication_path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("the publication directory opens"),
        );
        let publication_lock = lock_publication_directory(&publication, FlockOperation::LockShared)
            .expect("the fixture holds an active publication lock");
        let replacement_root = root.path().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::channel();
        let replacement = std::thread::spawn(move || {
            let outcome = open_bound_fixture(
                &replacement_root,
                PRIMARY_NAMESPACE,
                NamespaceBindingState::New,
            )
            .map(|_| ());
            sender
                .send(outcome)
                .expect("the replacement reports its construction outcome");
        });

        let blocked = receiver
            .recv_timeout(std::time::Duration::from_millis(100))
            .expect_err("the replacement cannot sweep while the lock is held");
        assert_eq!(blocked, std::sync::mpsc::RecvTimeoutError::Timeout);

        drop(publication_lock);
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the replacement completes after the lock is released")
            .expect("the replacement store opens");
        replacement
            .join()
            .expect("the replacement construction thread completes");
    }
}
