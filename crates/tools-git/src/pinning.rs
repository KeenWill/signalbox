use std::{
    collections::{BTreeMap, HashSet},
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
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::construction::LocalGitToolsConstructionError;
use crate::descriptor::{
    FileIdentity, FileSnapshotIdentity, RepositoryIdentity, descriptor_path,
    descriptor_path_from_fd, file_identity, file_snapshot_identity,
    unsupported_control_files_are_absent,
};
use crate::failure::LocalGitFailure;
use crate::layout::{
    object_id_bytes, open_repository_config_at, open_repository_head_at, open_repository_refs_at,
    parse_full_object_id_bytes, reject_administrative_symlinks, validate_live_shallow,
};
use crate::limits::{
    MAX_LOOSE_OBJECT_HEADER_BYTES, MAX_OBJECT_BYTES, MAX_OBJECT_DATABASE_BYTES,
    MAX_PACK_FILE_BYTES, MAX_REPOSITORY_CONFIG_BYTES, MAX_REPOSITORY_INSPECTIONS,
};
use crate::pack_install::OBJECT_PUBLICATION_LOCK;

pub(super) struct PinnedRepository {
    root_path: PathBuf,
    pub(super) root: fs::File,
    pub(super) git_directory: fs::File,
    _refs: fs::File,
    _config: fs::File,
    config_snapshot: fs::File,
    config_identity: FileSnapshotIdentity,
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
    compressed_bytes: u64,
    objects: OwnedFd,
    pack: OwnedFd,
    objects_identity: FileIdentity,
    bindings: Vec<ObjectChildBinding>,
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
            Self::Loose {
                object_format,
                directory_prefix,
                filename_bytes,
            } => {
                let mut claimed_object_id = directory_prefix.to_vec();
                claimed_object_id.extend_from_slice(name.as_bytes());
                name.as_bytes().len() == filename_bytes
                    && parse_full_object_id_bytes(&claimed_object_id, object_format).is_some()
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

    pub(super) fn open_repository_shell(&self) -> Result<RepositoryShell, LocalGitFailure> {
        self.validate_supported_layout()?;
        let repository = open_pinned_repository(&self.config_snapshot, self.object_format)
            .map_err(|_| LocalGitFailure::Repository)?;
        self.validate_supported_layout()?;
        Ok(repository)
    }

    pub(super) fn git_path(&self, path: &str) -> PathBuf {
        descriptor_path(&self.git_directory).join(path)
    }

    pub(super) fn object_id_bytes(&self) -> usize {
        object_id_bytes(self.object_format)
    }

    pub(super) fn validate_supported_layout(&self) -> Result<(), LocalGitFailure> {
        let head = open_repository_head_at(&self.git_directory, self.object_format)
            .map_err(|_| LocalGitFailure::Repository)?;
        validate_supported_layout(
            &self.root_path,
            &self.root,
            &self.git_directory,
            &self._refs,
            &self.config_snapshot,
            self.config_identity,
            head.identity,
            &head.bytes,
            self.object_format,
        )
    }

    pub(super) fn validate_object_layout(&self) -> Result<(), LocalGitFailure> {
        self.validate_supported_layout()
    }

    pub(super) fn operation_guard(&self) -> Result<RepositoryOperationGuard, LocalGitFailure> {
        self.validate_supported_layout()?;
        let head = open_repository_head_at(&self.git_directory, self.object_format)
            .map_err(|_| LocalGitFailure::Repository)?;
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
            _head: head.source,
            head_identity: head.identity,
            head_bytes: head.bytes,
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
    let administrative_directory = dup(git_directory).map_err(|_| LocalGitFailure::Repository)?;
    reject_administrative_symlinks(&administrative_directory, object_format)
        .map_err(|_| LocalGitFailure::Repository)?;
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
        Self::capture_with_hooks(authority, || {}, || {})
    }

    fn capture_with_hook<AfterScan: FnOnce()>(
        authority: &PinnedRepository,
        after_scan: AfterScan,
    ) -> Result<Self, LocalGitFailure> {
        Self::capture_with_hooks(authority, after_scan, || {})
    }

    fn capture_with_hooks<AfterScan: FnOnce(), AfterFinalBindings: FnOnce()>(
        authority: &PinnedRepository,
        after_scan: AfterScan,
        after_final_bindings: AfterFinalBindings,
    ) -> Result<Self, LocalGitFailure> {
        authority.validate_object_layout()?;
        let objects = openat(
            &authority.git_directory,
            "objects",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        let objects_identity = owned_directory_identity(&objects)?;
        let directory = tempfile::tempdir().map_err(|_| LocalGitFailure::Operation)?;
        fs::create_dir(directory.path().join("pack")).map_err(|_| LocalGitFailure::Operation)?;
        let loose_name_bytes = authority
            .object_id_bytes()
            .saturating_mul(2)
            .saturating_sub(2);
        let mut inspected = 0_usize;
        let mut captured_bytes = 0_u64;
        let mut pinned_children = Vec::new();
        let mut pinned_pack = None;
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
                pinned_pack = Some(pack);
                continue;
            }
            if bytes.len() != 2
                || name
                    .to_str()
                    .is_none_or(|prefix| gix_hash::Prefix::from_hex_nonempty(prefix).is_err())
            {
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
        let snapshot = Self {
            directory,
            compressed_bytes: captured_bytes,
            objects: dup(&objects).map_err(|_| LocalGitFailure::Repository)?,
            pack: pinned_pack.ok_or(LocalGitFailure::Repository)?,
            objects_identity,
            bindings: pinned_children,
        };
        snapshot.validate_object_sizes(authority.object_format)?;
        validate_owned_directory_binding(
            &authority.git_directory,
            OsStr::new("objects"),
            &objects,
        )?;
        validate_object_child_bindings(&objects, &snapshot.bindings)?;
        authority.validate_object_layout()?;
        validate_owned_directory_binding(
            &authority.git_directory,
            OsStr::new("objects"),
            &objects,
        )?;
        validate_object_child_bindings(&objects, &snapshot.bindings)?;
        after_final_bindings();
        authority.validate_object_layout()?;
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(super) fn capture_with_test_hook<AfterScan: FnOnce()>(
        authority: &PinnedRepository,
        after_scan: AfterScan,
    ) -> Result<Self, LocalGitFailure> {
        Self::capture_with_hook(authority, after_scan)
    }

    #[cfg(test)]
    pub(super) fn capture_with_post_bindings_test_hook<AfterFinalBindings: FnOnce()>(
        authority: &PinnedRepository,
        after_final_bindings: AfterFinalBindings,
    ) -> Result<Self, LocalGitFailure> {
        Self::capture_with_hooks(authority, || {}, after_final_bindings)
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

    pub(super) fn compressed_bytes(&self) -> u64 {
        self.compressed_bytes
    }

    pub(super) fn pack_directory(&self) -> &OwnedFd {
        &self.pack
    }

    pub(super) fn validate_live(
        &self,
        authority: &PinnedRepository,
    ) -> Result<(), LocalGitFailure> {
        authority.validate_object_layout()?;
        let objects = openat(
            &authority.git_directory,
            "objects",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        if owned_directory_identity(&objects)? != self.objects_identity {
            return Err(LocalGitFailure::Repository);
        }
        if owned_directory_identity(&self.objects)? != self.objects_identity {
            return Err(LocalGitFailure::Repository);
        }
        validate_retained_object_child_bindings(&objects, &self.bindings)?;
        authority.validate_object_layout()?;
        if owned_directory_identity(&objects)? != self.objects_identity {
            return Err(LocalGitFailure::Repository);
        }
        validate_retained_object_child_bindings(&objects, &self.bindings)
    }

    fn validate_object_sizes(&self, object_format: ObjectFormat) -> Result<(), LocalGitFailure> {
        let object_database =
            Odb::new_ext(object_format).map_err(|_| LocalGitFailure::Repository)?;
        self.add_to(&object_database)?;
        let object_ids = self.snapshot_object_ids(object_format)?;
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

    fn snapshot_object_ids(
        &self,
        object_format: ObjectFormat,
    ) -> Result<Vec<git2::Oid>, LocalGitFailure> {
        let mut object_ids = HashSet::new();
        for binding in &self.bindings {
            if binding.name == OsStr::new("pack") {
                collect_packed_object_ids(
                    &self.directory.path().join("pack"),
                    &binding.leaves,
                    object_format,
                    &mut object_ids,
                )?;
            } else {
                for leaf in &binding.leaves {
                    let mut object_id = binding.name.as_bytes().to_vec();
                    object_id.extend_from_slice(leaf.name.as_bytes());
                    let object_id = parse_full_object_id_bytes(&object_id, object_format)
                        .ok_or(LocalGitFailure::Repository)?;
                    insert_bounded_object_id(&mut object_ids, object_id)?;
                }
            }
        }
        Ok(object_ids.into_iter().collect())
    }
}

struct PackPair<'binding> {
    pack: Option<&'binding ObjectLeafBinding>,
    index: Option<&'binding ObjectLeafBinding>,
    object_id: git2::Oid,
}

fn collect_packed_object_ids(
    directory: &Path,
    leaves: &[ObjectLeafBinding],
    object_format: ObjectFormat,
    object_ids: &mut HashSet<git2::Oid>,
) -> Result<(), LocalGitFailure> {
    let mut pairs = BTreeMap::<Vec<u8>, PackPair<'_>>::new();
    for leaf in leaves {
        if leaf.name == OsStr::new(OBJECT_PUBLICATION_LOCK) {
            continue;
        }
        let name = leaf.name.as_bytes();
        let (stem, is_index) = if let Some(stem) = name.strip_suffix(b".pack") {
            (stem, false)
        } else if let Some(stem) = name.strip_suffix(b".idx") {
            (stem, true)
        } else {
            return Err(LocalGitFailure::Repository);
        };
        let object_id = stem
            .strip_prefix(b"pack-")
            .and_then(|value| parse_full_object_id_bytes(value, object_format))
            .ok_or(LocalGitFailure::Repository)?;
        let pair = pairs.entry(stem.to_vec()).or_insert(PackPair {
            pack: None,
            index: None,
            object_id,
        });
        if pair.object_id != object_id {
            return Err(LocalGitFailure::Repository);
        }
        let slot = if is_index {
            &mut pair.index
        } else {
            &mut pair.pack
        };
        if slot.replace(leaf).is_some() {
            return Err(LocalGitFailure::Repository);
        }
    }
    for pair in pairs.values() {
        let pack = pair.pack.ok_or(LocalGitFailure::Repository)?;
        let index = pair.index.ok_or(LocalGitFailure::Repository)?;
        let pack_bytes =
            fs::read(directory.join(&pack.name)).map_err(|_| LocalGitFailure::Repository)?;
        let packed_object_count = validate_pack_file(&pack_bytes, pair.object_id, object_format)?;
        let index_bytes =
            fs::read(directory.join(&index.name)).map_err(|_| LocalGitFailure::Repository)?;
        let indexed = parse_pack_index(&index_bytes, pair.object_id, object_format)?;
        if indexed.len() != packed_object_count {
            return Err(LocalGitFailure::Repository);
        }
        for object_id in indexed {
            insert_bounded_object_id(object_ids, object_id)?;
        }
    }
    Ok(())
}

fn insert_bounded_object_id(
    object_ids: &mut HashSet<git2::Oid>,
    object_id: git2::Oid,
) -> Result<(), LocalGitFailure> {
    object_ids.insert(object_id);
    if object_ids.len() > MAX_REPOSITORY_INSPECTIONS {
        Err(LocalGitFailure::Repository)
    } else {
        Ok(())
    }
}

pub(super) fn validate_pack_file(
    bytes: &[u8],
    expected_checksum: git2::Oid,
    object_format: ObjectFormat,
) -> Result<usize, LocalGitFailure> {
    let object_id_bytes = object_id_bytes(object_format);
    const PACK_HEADER_BYTES: usize = 12;
    if bytes.len() < PACK_HEADER_BYTES + object_id_bytes {
        return Err(LocalGitFailure::Repository);
    }
    let trailer_start = bytes
        .len()
        .checked_sub(object_id_bytes)
        .ok_or(LocalGitFailure::Repository)?;
    let trailer = bytes
        .get(trailer_start..)
        .ok_or(LocalGitFailure::Repository)?;
    let version = bytes
        .get(4..8)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(LocalGitFailure::Repository)?;
    let object_count = bytes
        .get(8..12)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_be_bytes)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(LocalGitFailure::Repository)?;
    if bytes.get(..4) != Some(b"PACK")
        || !matches!(version, 2 | 3)
        || trailer != expected_checksum.as_bytes()
        || object_digest(object_format, &bytes[..trailer_start]) != trailer
    {
        return Err(LocalGitFailure::Repository);
    }
    Ok(object_count)
}

pub(super) fn parse_pack_index(
    bytes: &[u8],
    expected_pack_checksum: git2::Oid,
    object_format: ObjectFormat,
) -> Result<Vec<git2::Oid>, LocalGitFailure> {
    const HEADER_BYTES: usize = 8;
    const FANOUT_BYTES: usize = 256 * 4;
    let object_id_bytes = object_id_bytes(object_format);
    if bytes.get(..4) != Some(&[0xff, b't', b'O', b'c'])
        || bytes.get(4..8) != Some(&2_u32.to_be_bytes())
    {
        return Err(LocalGitFailure::Repository);
    }
    let fanout = bytes
        .get(HEADER_BYTES..HEADER_BYTES + FANOUT_BYTES)
        .ok_or(LocalGitFailure::Repository)?;
    let count = read_be_u32(fanout, 255 * 4)? as usize;
    if count > MAX_REPOSITORY_INSPECTIONS {
        return Err(LocalGitFailure::Repository);
    }
    let object_table_start = HEADER_BYTES + FANOUT_BYTES;
    let object_table_bytes = count
        .checked_mul(object_id_bytes)
        .ok_or(LocalGitFailure::Repository)?;
    let object_table_end = object_table_start
        .checked_add(object_table_bytes)
        .ok_or(LocalGitFailure::Repository)?;
    let object_table = bytes
        .get(object_table_start..object_table_end)
        .ok_or(LocalGitFailure::Repository)?;
    let per_object_table_bytes = count.checked_mul(4).ok_or(LocalGitFailure::Repository)?;
    let offset_start = object_table_end
        .checked_add(per_object_table_bytes)
        .ok_or(LocalGitFailure::Repository)?;
    let offset_end = offset_start
        .checked_add(per_object_table_bytes)
        .ok_or(LocalGitFailure::Repository)?;
    let offsets = bytes
        .get(offset_start..offset_end)
        .ok_or(LocalGitFailure::Repository)?;
    let mut large_offsets = 0_usize;
    for offset in offsets.chunks_exact(4) {
        if read_be_u32(offset, 0)? >> 31 == 1 {
            large_offsets = large_offsets.saturating_add(1);
        }
    }
    let large_offset_bytes = large_offsets
        .checked_mul(8)
        .ok_or(LocalGitFailure::Repository)?;
    let checksum_start = offset_end
        .checked_add(large_offset_bytes)
        .ok_or(LocalGitFailure::Repository)?;
    let expected_length = checksum_start
        .checked_add(
            object_id_bytes
                .checked_mul(2)
                .ok_or(LocalGitFailure::Repository)?,
        )
        .ok_or(LocalGitFailure::Repository)?;
    if bytes.len() != expected_length {
        return Err(LocalGitFailure::Repository);
    }
    let pack_checksum = bytes
        .get(checksum_start..checksum_start + object_id_bytes)
        .ok_or(LocalGitFailure::Repository)?;
    let index_checksum = bytes
        .get(checksum_start + object_id_bytes..)
        .ok_or(LocalGitFailure::Repository)?;
    if pack_checksum != expected_pack_checksum.as_bytes()
        || object_digest(object_format, &bytes[..checksum_start + object_id_bytes])
            != index_checksum
    {
        return Err(LocalGitFailure::Repository);
    }
    let mut object_ids = Vec::with_capacity(count);
    let mut previous = None;
    let mut observed_fanout = [0_u32; 256];
    for raw in object_table.chunks_exact(object_id_bytes) {
        if previous.is_some_and(|previous: &[u8]| previous >= raw) {
            return Err(LocalGitFailure::Repository);
        }
        observed_fanout[raw[0] as usize] = observed_fanout[raw[0] as usize].saturating_add(1);
        object_ids.push(git2::Oid::from_bytes(raw).map_err(|_| LocalGitFailure::Repository)?);
        previous = Some(raw);
    }
    let mut cumulative = 0_u32;
    for (position, observed) in observed_fanout.into_iter().enumerate() {
        cumulative = cumulative
            .checked_add(observed)
            .ok_or(LocalGitFailure::Repository)?;
        if read_be_u32(fanout, position * 4)? != cumulative {
            return Err(LocalGitFailure::Repository);
        }
    }
    Ok(object_ids)
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Result<u32, LocalGitFailure> {
    bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(LocalGitFailure::Repository)
}

fn object_digest(object_format: ObjectFormat, bytes: &[u8]) -> Vec<u8> {
    match object_format {
        ObjectFormat::Sha1 => Sha1::digest(bytes).to_vec(),
        ObjectFormat::Sha256 => Sha256::digest(bytes).to_vec(),
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
    live_object_database_bytes_with_hook(authority, || {})
}

fn live_object_database_bytes_with_hook<AfterScan: FnOnce()>(
    authority: &PinnedRepository,
    after_scan: AfterScan,
) -> Result<u64, LocalGitFailure> {
    PinnedObjectDatabase::capture_with_hook(authority, after_scan)
        .map(|snapshot| snapshot.compressed_bytes)
}

#[cfg(test)]
pub(super) fn live_object_database_bytes_with_test_hook<AfterScan: FnOnce()>(
    authority: &PinnedRepository,
    after_scan: AfterScan,
) -> Result<u64, LocalGitFailure> {
    live_object_database_bytes_with_hook(authority, after_scan)
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

fn validate_retained_object_child_bindings(
    objects: &OwnedFd,
    expected: &[ObjectChildBinding],
) -> Result<(), LocalGitFailure> {
    let mut inspected = 0_usize;
    for binding in expected {
        inspected = inspected.saturating_add(1);
        if inspected > MAX_REPOSITORY_INSPECTIONS {
            return Err(LocalGitFailure::Repository);
        }
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
        for leaf in &binding.leaves {
            inspected = inspected.saturating_add(1);
            if inspected > MAX_REPOSITORY_INSPECTIONS {
                return Err(LocalGitFailure::Repository);
            }
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

fn validate_loose_object(
    file: &mut fs::File,
    object_format: ObjectFormat,
    directory_prefix: [u8; 2],
    filename: &OsStr,
) -> Result<(), LocalGitFailure> {
    let compressed_length = file
        .metadata()
        .map_err(|_| LocalGitFailure::Repository)?
        .len();
    let decoded_limit = MAX_LOOSE_OBJECT_HEADER_BYTES
        .saturating_add(MAX_OBJECT_BYTES)
        .saturating_add(1);
    let mut decoded = Vec::with_capacity(decoded_limit);
    let mut decoder = ZlibDecoder::new(&mut *file);
    Read::by_ref(&mut decoder)
        .take((decoded_limit + 1) as u64)
        .read_to_end(&mut decoded)
        .map_err(|_| LocalGitFailure::Repository)?;
    let consumed = decoder.total_in();
    drop(decoder);
    file.rewind().map_err(|_| LocalGitFailure::Repository)?;
    if decoded.len() > decoded_limit || consumed != compressed_length {
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

pub(super) fn repository_ignorecase(repository: &Repository) -> Result<bool, LocalGitFailure> {
    let config = repository
        .config()
        .map_err(|_| LocalGitFailure::Repository)?;
    match config.get_bool("core.ignorecase") {
        Ok(ignorecase) => Ok(ignorecase),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(false),
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
