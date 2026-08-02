use std::{
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{Read, Seek},
    ops::{Deref, DerefMut},
    os::{
        fd::{AsFd, OwnedFd},
        unix::ffi::OsStrExt,
        unix::fs::FileExt,
    },
    path::{Path, PathBuf},
    sync::Mutex,
};

use flate2::read::ZlibDecoder;
use git2::{Config, ErrorCode, ObjectFormat, Odb, Repository, RepositoryInitOptions};
use rustix::{
    fs::{CWD, Mode, OFlags, openat},
    io::dup,
};
use sha2::{Digest, Sha256};

use crate::construction::LocalGitToolsConstructionError;
use crate::descriptor::{
    FileIdentity, FileSnapshotIdentity, RepositoryIdentity, descriptor_path,
    descriptor_path_from_fd, file_identity, file_snapshot_identity,
    unsupported_control_files_are_absent,
};
use crate::failure::LocalGitFailure;
use crate::layout::{
    open_repository_config_at, open_repository_head_at, open_repository_refs_at,
    validate_live_shallow,
};
use crate::limits::{
    MAX_LOOSE_OBJECT_HEADER_BYTES, MAX_OBJECT_BYTES, MAX_OBJECT_DATABASE_BYTES,
    MAX_PACK_FILE_BYTES, MAX_REPOSITORY_CONFIG_BYTES, MAX_REPOSITORY_INSPECTIONS,
};

pub(super) struct PinnedRepository {
    root_path: PathBuf,
    pub(super) root: fs::File,
    pub(super) git_directory: fs::File,
    _refs: fs::File,
    _config: fs::File,
    config_snapshot: fs::File,
    config_identity: FileSnapshotIdentity,
    _head: fs::File,
    head_identity: FileSnapshotIdentity,
    head_bytes: Vec<u8>,
    pub(super) object_format: ObjectFormat,
    repository: Mutex<RepositoryShell>,
}

pub(super) struct RepositoryOperationGuard {
    root_path: PathBuf,
    root: fs::File,
    git_directory: fs::File,
    _refs: fs::File,
    _config: fs::File,
    config_snapshot: fs::File,
    config_identity: FileSnapshotIdentity,
    _head: fs::File,
    head_identity: FileSnapshotIdentity,
    head_bytes: Vec<u8>,
    object_format: ObjectFormat,
}

pub(super) struct RepositoryShell {
    repository: Repository,
    _directory: tempfile::TempDir,
}

pub(super) struct PinnedObjectDatabase {
    pub(super) directory: tempfile::TempDir,
}

struct ObjectChildBinding {
    name: OsString,
    identity: FileIdentity,
    leaves: Vec<ObjectLeafBinding>,
}

struct ObjectLeafBinding {
    name: OsString,
    snapshot: FileSnapshotIdentity,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ObjectDirectoryKind {
    Loose {
        object_format: ObjectFormat,
        directory_prefix: [u8; 2],
        filename_bytes: usize,
    },
    Pack,
}

impl ObjectDirectoryKind {
    fn validates_name(self, name: &OsStr) -> bool {
        match self {
            Self::Loose { filename_bytes, .. } => {
                name.as_bytes().len() == filename_bytes
                    && name.as_bytes().iter().all(u8::is_ascii_hexdigit)
            }
            Self::Pack => true,
        }
    }

    fn compressed_file_limit(self) -> usize {
        match self {
            Self::Loose { .. } => MAX_OBJECT_BYTES.saturating_mul(2),
            Self::Pack => MAX_PACK_FILE_BYTES,
        }
    }

    fn validate_content(self, file: &mut fs::File, name: &OsStr) -> Result<(), LocalGitFailure> {
        match self {
            Self::Loose {
                object_format,
                directory_prefix,
                ..
            } => validate_loose_object(file, object_format, directory_prefix, name),
            Self::Pack => Ok(()),
        }
    }
}

impl fmt::Debug for PinnedRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedRepository")
            .finish_non_exhaustive()
    }
}

impl Deref for RepositoryShell {
    type Target = Repository;

    fn deref(&self) -> &Self::Target {
        &self.repository
    }
}

impl DerefMut for RepositoryShell {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.repository
    }
}

impl PinnedRepository {
    pub(super) fn open(
        root_path: &Path,
        expected: RepositoryIdentity,
    ) -> Result<Self, LocalGitToolsConstructionError> {
        Self::open_with_hook(root_path, expected, || {})
    }

    pub(super) fn open_with_hook<Hook>(
        root_path: &Path,
        expected: RepositoryIdentity,
        after_git_directory_open: Hook,
    ) -> Result<Self, LocalGitToolsConstructionError>
    where
        Hook: FnOnce(),
    {
        Self::open_with_hooks(root_path, expected, after_git_directory_open, || {})
    }

    pub(super) fn open_with_hooks<GitDirectoryHook, ConfigHook>(
        root_path: &Path,
        expected: RepositoryIdentity,
        after_git_directory_open: GitDirectoryHook,
        after_config_snapshot: ConfigHook,
    ) -> Result<Self, LocalGitToolsConstructionError>
    where
        GitDirectoryHook: FnOnce(),
        ConfigHook: FnOnce(),
    {
        let root = fs::File::from(
            openat(
                CWD,
                root_path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitToolsConstructionError::Repository)?,
        );
        let git_directory = fs::File::from(
            openat(
                &root,
                ".git",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitToolsConstructionError::Repository)?,
        );
        after_git_directory_open();
        unsupported_control_files_are_absent(git_directory.as_fd())
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let config = open_repository_config_at(&git_directory)?;
        let head = open_repository_head_at(&git_directory, config.object_format)?;
        let refs = open_repository_refs_at(&git_directory)?;
        after_config_snapshot();
        unsupported_control_files_are_absent(git_directory.as_fd())
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let observed = RepositoryIdentity {
            root: file_identity(
                &root
                    .metadata()
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?,
            ),
            git_directory: file_identity(
                &git_directory
                    .metadata()
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?,
            ),
            refs: file_identity(
                &refs
                    .metadata()
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?,
            ),
            config: file_identity(
                &config
                    .source
                    .metadata()
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?,
            ),
            head: head.identity.file,
        };
        if observed != expected {
            return Err(LocalGitToolsConstructionError::Repository);
        }
        validate_directory_binding(&root, OsStr::new(".git"), &git_directory)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        validate_directory_binding(&git_directory, OsStr::new("refs"), &refs)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let repository = open_pinned_repository(&config.snapshot, config.object_format)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        unsupported_control_files_are_absent(git_directory.as_fd())
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let authority = Self {
            root_path: root_path.to_owned(),
            root,
            git_directory,
            _refs: refs,
            _config: config.source,
            config_snapshot: config.snapshot,
            config_identity: config.identity,
            _head: head.source,
            head_identity: head.identity,
            head_bytes: head.bytes,
            object_format: config.object_format,
            repository: Mutex::new(repository),
        };
        authority
            .validate_supported_layout()
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        Ok(authority)
    }

    pub(super) fn repository(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, RepositoryShell>, LocalGitFailure> {
        self.validate_supported_layout()?;
        self.repository
            .lock()
            .map_err(|_| LocalGitFailure::Repository)
    }

    pub(super) fn git_path(&self, path: &str) -> PathBuf {
        descriptor_path(&self.git_directory).join(path)
    }

    pub(super) fn object_id_bytes(&self) -> usize {
        match self.object_format {
            ObjectFormat::Sha1 => 20,
            ObjectFormat::Sha256 => 32,
        }
    }

    pub(super) fn validate_supported_layout(&self) -> Result<(), LocalGitFailure> {
        validate_supported_layout(
            &self.root_path,
            &self.root,
            &self.git_directory,
            &self._refs,
            &self.config_snapshot,
            self.config_identity,
            self.head_identity,
            &self.head_bytes,
            self.object_format,
        )
    }

    pub(super) fn validate_object_layout(&self) -> Result<(), LocalGitFailure> {
        self.validate_supported_layout()
    }

    pub(super) fn operation_guard(&self) -> Result<RepositoryOperationGuard, LocalGitFailure> {
        self.validate_supported_layout()?;
        let guard = RepositoryOperationGuard {
            root_path: self.root_path.clone(),
            root: self
                .root
                .try_clone()
                .map_err(|_| LocalGitFailure::Operation)?,
            git_directory: self
                .git_directory
                .try_clone()
                .map_err(|_| LocalGitFailure::Operation)?,
            _refs: self
                ._refs
                .try_clone()
                .map_err(|_| LocalGitFailure::Operation)?,
            _config: self
                ._config
                .try_clone()
                .map_err(|_| LocalGitFailure::Operation)?,
            config_snapshot: self
                .config_snapshot
                .try_clone()
                .map_err(|_| LocalGitFailure::Operation)?,
            config_identity: self.config_identity,
            _head: self
                ._head
                .try_clone()
                .map_err(|_| LocalGitFailure::Operation)?,
            head_identity: self.head_identity,
            head_bytes: self.head_bytes.clone(),
            object_format: self.object_format,
        };
        guard.validate_supported_layout()?;
        Ok(guard)
    }
}

impl RepositoryOperationGuard {
    pub(super) fn validate_supported_layout(&self) -> Result<(), LocalGitFailure> {
        validate_supported_layout(
            &self.root_path,
            &self.root,
            &self.git_directory,
            &self._refs,
            &self.config_snapshot,
            self.config_identity,
            self.head_identity,
            &self.head_bytes,
            self.object_format,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_supported_layout(
    root_path: &Path,
    root: &fs::File,
    git_directory: &fs::File,
    refs: &fs::File,
    config_snapshot: &fs::File,
    config_identity: FileSnapshotIdentity,
    head_identity: FileSnapshotIdentity,
    head_bytes: &[u8],
    object_format: ObjectFormat,
) -> Result<(), LocalGitFailure> {
    validate_root_path_binding(root_path, root)?;
    validate_directory_binding(root, OsStr::new(".git"), git_directory)?;
    validate_directory_binding(git_directory, OsStr::new("refs"), refs)?;
    validate_head_at(git_directory, object_format, head_identity, head_bytes)?;
    unsupported_control_files_are_absent(git_directory.as_fd())?;
    validate_live_shallow(git_directory, object_format)?;
    validate_config_at(git_directory, config_snapshot, config_identity)?;
    // Repeat the mutable-file checks to bracket config validation and catch a
    // concurrent change that occurs between either side of the sequence.
    validate_head_at(git_directory, object_format, head_identity, head_bytes)?;
    validate_live_shallow(git_directory, object_format)?;
    unsupported_control_files_are_absent(git_directory.as_fd())?;
    validate_head_at(git_directory, object_format, head_identity, head_bytes)?;
    validate_directory_binding(git_directory, OsStr::new("refs"), refs)?;
    validate_directory_binding(root, OsStr::new(".git"), git_directory)?;
    validate_root_path_binding(root_path, root)
}

fn validate_root_path_binding(root_path: &Path, root: &fs::File) -> Result<(), LocalGitFailure> {
    let expected = file_identity(&root.metadata().map_err(|_| LocalGitFailure::Repository)?);
    let current = fs::symlink_metadata(root_path).map_err(|_| LocalGitFailure::Repository)?;
    if current.file_type().is_symlink() || !current.is_dir() || file_identity(&current) != expected
    {
        return Err(LocalGitFailure::Repository);
    }
    Ok(())
}

fn validate_directory_binding(
    parent: &fs::File,
    name: &OsStr,
    pinned: &fs::File,
) -> Result<(), LocalGitFailure> {
    let expected = file_identity(&pinned.metadata().map_err(|_| LocalGitFailure::Repository)?);
    let current = fs::File::from(
        openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?,
    );
    if file_identity(
        &current
            .metadata()
            .map_err(|_| LocalGitFailure::Repository)?,
    ) != expected
    {
        return Err(LocalGitFailure::Repository);
    }
    Ok(())
}

fn validate_head_at(
    git_directory: &fs::File,
    object_format: ObjectFormat,
    expected_identity: FileSnapshotIdentity,
    expected_bytes: &[u8],
) -> Result<(), LocalGitFailure> {
    let current = open_repository_head_at(git_directory, object_format)
        .map_err(|_| LocalGitFailure::Repository)?;
    if current.identity != expected_identity || current.bytes != expected_bytes {
        return Err(LocalGitFailure::Repository);
    }
    Ok(())
}

fn validate_config_at(
    git_directory: &fs::File,
    config_snapshot: &fs::File,
    config_identity: FileSnapshotIdentity,
) -> Result<(), LocalGitFailure> {
    let current =
        open_repository_config_at(git_directory).map_err(|_| LocalGitFailure::Repository)?;
    if current.identity != config_identity
        || config_snapshot_bytes(&current.snapshot)? != config_snapshot_bytes(config_snapshot)?
    {
        return Err(LocalGitFailure::Repository);
    }
    let path_descriptor = openat(
        git_directory,
        "config",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    let current_identity = file_snapshot_identity(
        &current
            .source
            .metadata()
            .map_err(|_| LocalGitFailure::Repository)?,
    );
    let path_identity = file_snapshot_identity(
        &fs::File::from(path_descriptor)
            .metadata()
            .map_err(|_| LocalGitFailure::Repository)?,
    );
    if current_identity != config_identity || path_identity != config_identity {
        return Err(LocalGitFailure::Repository);
    }
    Ok(())
}

fn config_snapshot_bytes(file: &fs::File) -> Result<Vec<u8>, LocalGitFailure> {
    let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
    let length = usize::try_from(metadata.len())
        .ok()
        .filter(|length| *length <= MAX_REPOSITORY_CONFIG_BYTES)
        .ok_or(LocalGitFailure::Repository)?;
    let mut bytes = vec![0_u8; length];
    file.read_exact_at(&mut bytes, 0)
        .map_err(|_| LocalGitFailure::Repository)?;
    Ok(bytes)
}

impl PinnedObjectDatabase {
    pub(super) fn capture(authority: &PinnedRepository) -> Result<Self, LocalGitFailure> {
        Self::capture_with_hook(authority, || {})
    }

    fn capture_with_hook<AfterScan: FnOnce()>(
        authority: &PinnedRepository,
        after_scan: AfterScan,
    ) -> Result<Self, LocalGitFailure> {
        authority.validate_object_layout()?;
        let objects = openat(
            &authority.git_directory,
            "objects",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        let directory = tempfile::tempdir().map_err(|_| LocalGitFailure::Operation)?;
        fs::create_dir(directory.path().join("pack")).map_err(|_| LocalGitFailure::Operation)?;
        let loose_name_bytes = authority
            .object_id_bytes()
            .saturating_mul(2)
            .saturating_sub(2);
        let mut inspected = 0_usize;
        let mut captured_bytes = 0_u64;
        let mut pinned_children = Vec::new();
        for entry in fs::read_dir(descriptor_path_from_fd(&objects))
            .map_err(|_| LocalGitFailure::Repository)?
        {
            let entry = entry.map_err(|_| LocalGitFailure::Repository)?;
            inspected = inspected.saturating_add(1);
            if inspected > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
            let name = entry.file_name();
            let bytes = name.as_bytes();
            if name == OsStr::new("info") {
                continue;
            }
            if name == OsStr::new("pack") {
                let pack = openat(
                    &objects,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| LocalGitFailure::Repository)?;
                let identity = owned_directory_identity(&pack)?;
                let leaves = pin_object_directory(
                    &pack,
                    &directory.path().join("pack"),
                    &mut inspected,
                    &mut captured_bytes,
                    ObjectDirectoryKind::Pack,
                )?;
                pinned_children.push(ObjectChildBinding {
                    name,
                    identity,
                    leaves,
                });
                continue;
            }
            if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_hexdigit) {
                return Err(LocalGitFailure::Repository);
            }
            let directory_prefix = bytes.try_into().map_err(|_| LocalGitFailure::Repository)?;
            let loose_kind = ObjectDirectoryKind::Loose {
                object_format: authority.object_format,
                directory_prefix,
                filename_bytes: loose_name_bytes,
            };
            let loose = openat(
                &objects,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Repository)?;
            let identity = owned_directory_identity(&loose)?;
            let destination = directory.path().join(&name);
            fs::create_dir(&destination).map_err(|_| LocalGitFailure::Operation)?;
            let leaves = pin_object_directory(
                &loose,
                &destination,
                &mut inspected,
                &mut captured_bytes,
                loose_kind,
            )?;
            pinned_children.push(ObjectChildBinding {
                name,
                identity,
                leaves,
            });
        }
        after_scan();
        let snapshot = Self { directory };
        snapshot.validate_object_sizes(authority.object_format)?;
        validate_owned_directory_binding(
            &authority.git_directory,
            OsStr::new("objects"),
            &objects,
        )?;
        validate_object_child_bindings(&objects, &pinned_children)?;
        authority.validate_object_layout()?;
        validate_owned_directory_binding(
            &authority.git_directory,
            OsStr::new("objects"),
            &objects,
        )?;
        validate_object_child_bindings(&objects, &pinned_children)?;
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(super) fn capture_with_test_hook<AfterScan: FnOnce()>(
        authority: &PinnedRepository,
        after_scan: AfterScan,
    ) -> Result<Self, LocalGitFailure> {
        Self::capture_with_hook(authority, after_scan)
    }

    pub(super) fn add_to(&self, object_database: &Odb<'_>) -> Result<(), LocalGitFailure> {
        let path = self
            .directory
            .path()
            .to_str()
            .ok_or(LocalGitFailure::Operation)?;
        object_database
            .add_disk_alternate(path)
            .map_err(|_| LocalGitFailure::Operation)
    }

    fn validate_object_sizes(&self, object_format: ObjectFormat) -> Result<(), LocalGitFailure> {
        let object_database =
            Odb::new_ext(object_format).map_err(|_| LocalGitFailure::Repository)?;
        self.add_to(&object_database)?;
        let mut object_ids = Vec::new();
        let enumeration = object_database.foreach(|object_id| {
            object_ids.push(*object_id);
            object_ids.len() <= MAX_REPOSITORY_INSPECTIONS
        });
        if enumeration.is_err() || object_ids.len() > MAX_REPOSITORY_INSPECTIONS {
            return Err(LocalGitFailure::Repository);
        }
        let mut decoded_total = 0_usize;
        for object_id in object_ids {
            let (decoded_bytes, object_type) = object_database
                .read_header(object_id)
                .map_err(|_| LocalGitFailure::Repository)?;
            decoded_total = decoded_total
                .checked_add(decoded_bytes)
                .filter(|total| *total <= MAX_OBJECT_DATABASE_BYTES)
                .ok_or(LocalGitFailure::Repository)?;
            if decoded_bytes > MAX_OBJECT_BYTES {
                return Err(LocalGitFailure::Repository);
            }
            let object = object_database
                .read(object_id)
                .map_err(|_| LocalGitFailure::Repository)?;
            let verified_id = git2::Oid::hash_object_ext(object_type, object.data(), object_format)
                .map_err(|_| LocalGitFailure::Repository)?;
            if object.data().len() != decoded_bytes || verified_id != object_id {
                return Err(LocalGitFailure::Repository);
            }
        }
        Ok(())
    }
}

fn pin_object_directory(
    source: &OwnedFd,
    destination: &Path,
    inspected: &mut usize,
    captured_bytes: &mut u64,
    kind: ObjectDirectoryKind,
) -> Result<Vec<ObjectLeafBinding>, LocalGitFailure> {
    let mut bindings = Vec::new();
    for entry in
        fs::read_dir(descriptor_path_from_fd(source)).map_err(|_| LocalGitFailure::Repository)?
    {
        let entry = entry.map_err(|_| LocalGitFailure::Repository)?;
        *inspected = inspected.saturating_add(1);
        if *inspected > MAX_REPOSITORY_INSPECTIONS {
            return Err(LocalGitFailure::Repository);
        }
        let name = entry.file_name();
        if !kind.validates_name(&name) {
            return Err(LocalGitFailure::Repository);
        }
        let descriptor = openat(
            source,
            &name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        let mut file = fs::File::from(descriptor);
        let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
        let per_file_limit = kind.compressed_file_limit() as u64;
        if !metadata.is_file()
            || metadata.len() > per_file_limit
            || captured_bytes.saturating_add(metadata.len()) > MAX_OBJECT_DATABASE_BYTES as u64
        {
            return Err(LocalGitFailure::Repository);
        }
        let mut snapshot = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(destination.join(&name))
            .map_err(|_| LocalGitFailure::Operation)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut file).take(metadata.len().saturating_add(1)),
            &mut snapshot,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let after_copy = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
        if copied != metadata.len()
            || file_snapshot_identity(&metadata) != file_snapshot_identity(&after_copy)
        {
            return Err(LocalGitFailure::Repository);
        }
        snapshot.rewind().map_err(|_| LocalGitFailure::Operation)?;
        kind.validate_content(&mut snapshot, &name)?;
        let digest = object_leaf_digest(&mut snapshot, copied)?;
        bindings.push(ObjectLeafBinding {
            name,
            snapshot: file_snapshot_identity(&after_copy),
            digest,
        });
        *captured_bytes = captured_bytes.saturating_add(copied);
    }
    Ok(bindings)
}

fn object_leaf_digest(
    file: &mut fs::File,
    expected_length: u64,
) -> Result<[u8; 32], LocalGitFailure> {
    file.rewind().map_err(|_| LocalGitFailure::Repository)?;
    let mut remaining = expected_length;
    let mut buffer = [0_u8; 8192];
    let mut digest = Sha256::new();
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| LocalGitFailure::Repository)?;
        let read = file
            .read(&mut buffer[..requested])
            .map_err(|_| LocalGitFailure::Repository)?;
        if read == 0 {
            return Err(LocalGitFailure::Repository);
        }
        digest.update(&buffer[..read]);
        remaining = remaining.saturating_sub(read as u64);
    }
    let mut extra = [0_u8; 1];
    if file
        .read(&mut extra)
        .map_err(|_| LocalGitFailure::Repository)?
        != 0
    {
        return Err(LocalGitFailure::Repository);
    }
    Ok(digest.finalize().into())
}

pub(super) fn live_object_database_bytes(
    authority: &PinnedRepository,
) -> Result<u64, LocalGitFailure> {
    authority.validate_object_layout()?;
    let objects = openat(
        &authority.git_directory,
        "objects",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    let mut inspected = 0_usize;
    let mut bytes = 0_u64;
    let loose_name_bytes = authority
        .object_id_bytes()
        .saturating_mul(2)
        .saturating_sub(2);
    for entry in
        fs::read_dir(descriptor_path_from_fd(&objects)).map_err(|_| LocalGitFailure::Repository)?
    {
        let entry = entry.map_err(|_| LocalGitFailure::Repository)?;
        inspected = inspected.saturating_add(1);
        if inspected > MAX_REPOSITORY_INSPECTIONS {
            return Err(LocalGitFailure::Repository);
        }
        let name = entry.file_name();
        if name == OsStr::new("info") {
            continue;
        }
        if name == OsStr::new("pack") {
            let pack = openat(
                &objects,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Repository)?;
            measure_object_directory(&pack, &mut inspected, &mut bytes, ObjectDirectoryKind::Pack)?;
            continue;
        }
        if name.as_bytes().len() != 2 || !name.as_bytes().iter().all(u8::is_ascii_hexdigit) {
            return Err(LocalGitFailure::Repository);
        }
        let directory_prefix = name
            .as_bytes()
            .try_into()
            .map_err(|_| LocalGitFailure::Repository)?;
        let loose_kind = ObjectDirectoryKind::Loose {
            object_format: authority.object_format,
            directory_prefix,
            filename_bytes: loose_name_bytes,
        };
        let loose = openat(
            &objects,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        measure_object_directory(&loose, &mut inspected, &mut bytes, loose_kind)?;
    }
    validate_owned_directory_binding(&authority.git_directory, OsStr::new("objects"), &objects)?;
    authority.validate_object_layout()?;
    validate_owned_directory_binding(&authority.git_directory, OsStr::new("objects"), &objects)?;
    Ok(bytes)
}

fn validate_owned_directory_binding<Parent: AsFd>(
    parent: &Parent,
    name: &OsStr,
    pinned: &OwnedFd,
) -> Result<(), LocalGitFailure> {
    let pinned_file = fs::File::from(dup(pinned).map_err(|_| LocalGitFailure::Repository)?);
    let expected = file_identity(
        &pinned_file
            .metadata()
            .map_err(|_| LocalGitFailure::Repository)?,
    );
    let current = fs::File::from(
        openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?,
    );
    if file_identity(
        &current
            .metadata()
            .map_err(|_| LocalGitFailure::Repository)?,
    ) != expected
    {
        return Err(LocalGitFailure::Repository);
    }
    Ok(())
}

fn owned_directory_identity(directory: &OwnedFd) -> Result<FileIdentity, LocalGitFailure> {
    let file = fs::File::from(dup(directory).map_err(|_| LocalGitFailure::Repository)?);
    file.metadata()
        .map(|metadata| file_identity(&metadata))
        .map_err(|_| LocalGitFailure::Repository)
}

fn validate_object_child_bindings(
    objects: &OwnedFd,
    expected: &[ObjectChildBinding],
) -> Result<(), LocalGitFailure> {
    let mut expected_children = expected
        .iter()
        .map(|binding| binding.name.clone())
        .collect::<Vec<_>>();
    expected_children.sort();
    let mut inspected = 0_usize;
    let mut current_children = Vec::new();
    for entry in
        fs::read_dir(descriptor_path_from_fd(objects)).map_err(|_| LocalGitFailure::Repository)?
    {
        inspected = inspected.saturating_add(1);
        if inspected > MAX_REPOSITORY_INSPECTIONS {
            return Err(LocalGitFailure::Repository);
        }
        let name = entry.map_err(|_| LocalGitFailure::Repository)?.file_name();
        if name != OsStr::new("info") {
            current_children.push(name);
        }
    }
    current_children.sort();
    if current_children != expected_children {
        return Err(LocalGitFailure::Repository);
    }
    for binding in expected {
        let current = openat(
            objects,
            &binding.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        if owned_directory_identity(&current)? != binding.identity {
            return Err(LocalGitFailure::Repository);
        }
        let mut current_leaves = Vec::new();
        for entry in fs::read_dir(descriptor_path_from_fd(&current))
            .map_err(|_| LocalGitFailure::Repository)?
        {
            inspected = inspected.saturating_add(1);
            if inspected > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
            current_leaves.push(entry.map_err(|_| LocalGitFailure::Repository)?.file_name());
        }
        current_leaves.sort();
        let mut expected_leaves = binding
            .leaves
            .iter()
            .map(|leaf| leaf.name.clone())
            .collect::<Vec<_>>();
        expected_leaves.sort();
        if current_leaves != expected_leaves {
            return Err(LocalGitFailure::Repository);
        }
        for leaf in &binding.leaves {
            let descriptor = openat(
                &current,
                &leaf.name,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Repository)?;
            let mut file = fs::File::from(descriptor);
            let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
            if !metadata.is_file() || file_snapshot_identity(&metadata) != leaf.snapshot {
                return Err(LocalGitFailure::Repository);
            }
            let digest = object_leaf_digest(&mut file, leaf.snapshot.length)?;
            let after_read = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
            if file_snapshot_identity(&after_read) != leaf.snapshot || digest != leaf.digest {
                return Err(LocalGitFailure::Repository);
            }
        }
    }
    Ok(())
}

pub(super) fn measure_object_directory(
    directory: &OwnedFd,
    inspected: &mut usize,
    total_bytes: &mut u64,
    kind: ObjectDirectoryKind,
) -> Result<(), LocalGitFailure> {
    for entry in
        fs::read_dir(descriptor_path_from_fd(directory)).map_err(|_| LocalGitFailure::Repository)?
    {
        let entry = entry.map_err(|_| LocalGitFailure::Repository)?;
        *inspected = inspected.saturating_add(1);
        if *inspected > MAX_REPOSITORY_INSPECTIONS {
            return Err(LocalGitFailure::Repository);
        }
        let name = entry.file_name();
        if !kind.validates_name(&name) {
            return Err(LocalGitFailure::Repository);
        }
        let descriptor = openat(
            directory,
            &name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        let mut file = fs::File::from(descriptor);
        let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
        let per_file_limit = kind.compressed_file_limit() as u64;
        *total_bytes = total_bytes
            .checked_add(metadata.len())
            .filter(|bytes| *bytes <= MAX_OBJECT_DATABASE_BYTES as u64)
            .ok_or(LocalGitFailure::Repository)?;
        if !metadata.is_file() || metadata.len() > per_file_limit {
            return Err(LocalGitFailure::Repository);
        }
        kind.validate_content(&mut file, &name)?;
    }
    Ok(())
}

fn validate_loose_object(
    file: &mut fs::File,
    object_format: ObjectFormat,
    directory_prefix: [u8; 2],
    filename: &OsStr,
) -> Result<(), LocalGitFailure> {
    let decoded_limit = MAX_LOOSE_OBJECT_HEADER_BYTES
        .saturating_add(MAX_OBJECT_BYTES)
        .saturating_add(1);
    let mut decoded = Vec::with_capacity(decoded_limit);
    let mut decoder = ZlibDecoder::new(&mut *file);
    Read::by_ref(&mut decoder)
        .take((decoded_limit + 1) as u64)
        .read_to_end(&mut decoded)
        .map_err(|_| LocalGitFailure::Repository)?;
    drop(decoder);
    file.rewind().map_err(|_| LocalGitFailure::Repository)?;
    if decoded.len() > decoded_limit {
        return Err(LocalGitFailure::Repository);
    }
    let header_end = decoded
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(LocalGitFailure::Repository)?;
    if header_end > MAX_LOOSE_OBJECT_HEADER_BYTES {
        return Err(LocalGitFailure::Repository);
    }
    let header =
        std::str::from_utf8(&decoded[..header_end]).map_err(|_| LocalGitFailure::Repository)?;
    let (kind, declared_text) = header.split_once(' ').ok_or(LocalGitFailure::Repository)?;
    if !matches!(kind, "blob" | "tree" | "commit" | "tag") {
        return Err(LocalGitFailure::Repository);
    }
    let declared_bytes = declared_text
        .parse::<usize>()
        .ok()
        .filter(|bytes| *bytes <= MAX_OBJECT_BYTES)
        .ok_or(LocalGitFailure::Repository)?;
    if declared_bytes.to_string() != declared_text
        || decoded.len() != header_end.saturating_add(1).saturating_add(declared_bytes)
    {
        return Err(LocalGitFailure::Repository);
    }
    let object_type = match kind {
        "blob" => git2::ObjectType::Blob,
        "tree" => git2::ObjectType::Tree,
        "commit" => git2::ObjectType::Commit,
        "tag" => git2::ObjectType::Tag,
        _ => return Err(LocalGitFailure::Repository),
    };
    let object_id =
        git2::Oid::hash_object_ext(object_type, &decoded[header_end + 1..], object_format)
            .map_err(|_| LocalGitFailure::Repository)?;
    let mut claimed_object_id = directory_prefix.to_vec();
    claimed_object_id.extend_from_slice(filename.as_bytes());
    if object_id.to_string().as_bytes() != claimed_object_id {
        return Err(LocalGitFailure::Repository);
    }
    Ok(())
}

pub(super) fn open_pinned_repository(
    config: &fs::File,
    object_format: ObjectFormat,
) -> Result<RepositoryShell, git2::Error> {
    let directory =
        tempfile::tempdir().map_err(|error| git2::Error::from_str(&error.to_string()))?;
    let mut options = RepositoryInitOptions::new();
    options
        .bare(true)
        .no_reinit(true)
        .external_template(false)
        .initial_head("refs/heads/signalbox-pinned")
        .object_format(object_format);
    let repository = Repository::init_opts(directory.path(), &options)?;
    let config = Config::open(&descriptor_path(config))?;
    repository.set_config(&config)?;
    Ok(RepositoryShell {
        repository,
        _directory: directory,
    })
}

pub(super) fn repository_filemode(repository: &Repository) -> Result<bool, LocalGitFailure> {
    let config = repository
        .config()
        .map_err(|_| LocalGitFailure::Repository)?;
    match config.get_bool("core.filemode") {
        Ok(filemode) => Ok(filemode),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(true),
        Err(_) => Err(LocalGitFailure::Repository),
    }
}

pub(super) fn pin_optional_git_file(
    path: &Path,
    max_bytes: usize,
) -> Result<Option<fs::File>, LocalGitFailure> {
    match openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            let file = fs::File::from(descriptor);
            let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
            if metadata.is_file() && metadata.len() <= max_bytes as u64 {
                Ok(Some(file))
            } else {
                Err(LocalGitFailure::Repository)
            }
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(_) => Err(LocalGitFailure::Repository),
    }
}
