use std::{
    cell::RefCell,
    ffi::OsStr,
    fmt, fs,
    ops::{Deref, DerefMut},
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::FileExt,
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use git2::{Config, ErrorCode, ObjectFormat, Odb, Repository, RepositoryInitOptions};
use rustix::{
    fs::{CWD, Mode, OFlags, openat},
    io::dup,
};

use crate::construction::LocalGitToolsConstructionError;
use crate::descriptor::{
    FileSnapshotIdentity, RepositoryIdentity, descriptor_path, file_identity,
    file_snapshot_identity, unsupported_control_files_are_absent,
};
use crate::descriptor_identity::bind_still_holds;
use crate::failure::LocalGitFailure;
use crate::layout::{
    open_repository_config_at, open_repository_head_at, open_repository_refs_at,
    reject_administrative_symlinks, validate_live_shallow,
};
use crate::limits::MAX_REPOSITORY_CONFIG_BYTES;
use crate::push_objects::ObjectSource;

pub(super) struct PinnedRepository {
    pub(super) max_object_bytes: Option<usize>,
    root_path: PathBuf,
    pub(super) root: fs::File,
    pub(super) git_directory: fs::File,
    pub(super) worktree_directory: fs::File,
    administration: crate::repository_directories::AdministrationBinding,
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
    worktree_directory: fs::File,
    administration: crate::repository_directories::AdministrationBinding,
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
    pub(super) max_object_bytes: Option<usize>,
    repository: Repository,
    _directory: tempfile::TempDir,
    selected_objects: RefCell<Option<Arc<Mutex<ObjectSource>>>>,
}

pub(super) struct PinnedObjectDatabase {
    source: Arc<Mutex<ObjectSource>>,
    pack: OwnedFd,
}

impl RepositoryShell {
    pub(super) fn object_byte_limit(&self) -> usize {
        self.max_object_bytes.unwrap_or(usize::MAX)
    }
    pub(super) fn capture_objects_on_read(
        &self,
        authority: &PinnedRepository,
    ) -> Result<(), LocalGitFailure> {
        *self.selected_objects.borrow_mut() = Some(Arc::new(Mutex::new(ObjectSource::open(
            authority,
            std::time::Instant::now() + crate::push_executor::PUSH_PREPARATION_TIMEOUT,
        )?)));
        Ok(())
    }

    pub(super) fn validate_selected_objects(
        &self,
        authority: &PinnedRepository,
    ) -> Result<(), LocalGitFailure> {
        self.selected_objects
            .borrow()
            .as_ref()
            .ok_or(LocalGitFailure::Operation)?
            .lock()
            .map_err(|_| LocalGitFailure::Operation)?
            .validate(authority)
    }

    pub(super) fn set_odb(
        &self,
        database: &Odb<'_>,
        snapshot: &PinnedObjectDatabase,
    ) -> Result<(), git2::Error> {
        self.repository.set_odb(database)?;
        *self.selected_objects.borrow_mut() = Some(Arc::clone(&snapshot.source));
        Ok(())
    }

    pub(super) fn retain_selected_source(&self, source: ObjectSource) {
        *self.selected_objects.borrow_mut() = Some(Arc::new(Mutex::new(source)));
    }

    pub(super) fn store_content(
        &self,
        content: &mut crate::streamed_object::ObjectContent,
    ) -> Result<git2::Oid, LocalGitFailure> {
        let database = self
            .repository
            .odb()
            .map_err(|_| LocalGitFailure::Operation)?;
        self.selected_objects
            .borrow()
            .as_ref()
            .ok_or(LocalGitFailure::Operation)?
            .lock()
            .map_err(|_| LocalGitFailure::Operation)?
            .store(&database, content)
    }

    pub(super) fn object_content(
        &self,
        oid: git2::Oid,
    ) -> Result<crate::streamed_object::ObjectContent, LocalGitFailure> {
        self.read_object_header(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        if let Some(source) = self.selected_objects.borrow().as_ref()
            && let Some(content) = source
                .lock()
                .map_err(|_| LocalGitFailure::Operation)?
                .content(oid)?
        {
            return Ok(content);
        }
        let database = self
            .repository
            .odb()
            .map_err(|_| LocalGitFailure::Operation)?;
        let object = database.read(oid).map_err(|_| LocalGitFailure::Operation)?;
        crate::streamed_object::ObjectContent::decode(
            &mut object.data(),
            object.len(),
            object.kind(),
        )
    }

    pub(super) fn read_object_header(
        &self,
        oid: git2::Oid,
    ) -> Result<(usize, git2::ObjectType), git2::Error> {
        if let Some(source) = self.selected_objects.borrow().as_ref() {
            let mut source = source
                .lock()
                .map_err(|_| git2::Error::from_str("object source lock failed"))?;
            let database = self.repository.odb()?;
            source.capture(&database, oid).map_err(|_| {
                git2::Error::from_str("object read exceeds captured content bounds")
            })?;
            return database.read_header(oid);
        }
        self.repository.odb()?.read_header(oid)
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
        let directories = crate::repository_directories::AdministrationDirectories::open(&root)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let administration = directories.binding;
        let git_directory = directories.common;
        let worktree_directory = directories.worktree;
        after_git_directory_open();
        unsupported_control_files_are_absent(git_directory.as_fd())
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let config = open_repository_config_at(&git_directory)?;
        let head = open_repository_head_at(&worktree_directory, config.object_format)?;
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
            administration,
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
        crate::repository_directories::validate_binding(&root, administration)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        validate_directory_binding(&git_directory, OsStr::new("refs"), &refs)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let repository = open_pinned_repository(&config.snapshot, config.object_format)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        unsupported_control_files_are_absent(git_directory.as_fd())
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let authority = Self {
            max_object_bytes: None,
            root_path: root_path.to_owned(),
            root,
            git_directory,
            worktree_directory,
            administration,
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
        let mut repository = self
            .repository
            .lock()
            .map_err(|_| LocalGitFailure::Repository)?;
        repository.max_object_bytes = self.max_object_bytes;
        Ok(repository)
    }

    pub(super) fn open_repository_shell(&self) -> Result<RepositoryShell, LocalGitFailure> {
        self.validate_supported_layout()?;
        let mut repository = open_pinned_repository(&self.config_snapshot, self.object_format)
            .map_err(|_| LocalGitFailure::Repository)?;
        self.validate_supported_layout()?;
        repository.max_object_bytes = self.max_object_bytes;
        Ok(repository)
    }

    pub(super) fn administration_for(&self, path: &str) -> &fs::File {
        let reference = path.strip_prefix("logs/").unwrap_or(path);
        if reference.starts_with("refs/")
            && !["refs/bisect/", "refs/worktree/", "refs/rewritten/"]
                .iter()
                .any(|prefix| reference.starts_with(prefix))
        {
            &self.git_directory
        } else {
            &self.worktree_directory
        }
    }

    pub(super) fn git_path(&self, path: &str) -> PathBuf {
        descriptor_path(self.administration_for(path)).join(path)
    }

    pub(super) fn validate_supported_layout(&self) -> Result<(), LocalGitFailure> {
        let head = open_repository_head_at(&self.worktree_directory, self.object_format)
            .map_err(|_| LocalGitFailure::Repository)?;
        validate_supported_layout(
            &self.root_path,
            &self.root,
            &self.git_directory,
            &self.worktree_directory,
            self.administration,
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
        let head = open_repository_head_at(&self.worktree_directory, self.object_format)
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
            worktree_directory: self
                .worktree_directory
                .try_clone()
                .map_err(|_| LocalGitFailure::Operation)?,
            administration: self.administration,
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
            &self.worktree_directory,
            self.administration,
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
    worktree_directory: &fs::File,
    administration: crate::repository_directories::AdministrationBinding,
    refs: &fs::File,
    config_snapshot: &fs::File,
    config_identity: FileSnapshotIdentity,
    head_identity: FileSnapshotIdentity,
    head_bytes: &[u8],
    object_format: ObjectFormat,
) -> Result<(), LocalGitFailure> {
    validate_root_path_binding(root_path, root)?;
    crate::repository_directories::validate_binding(root, administration)?;
    validate_directory_binding(git_directory, OsStr::new("refs"), refs)?;
    validate_head_at(worktree_directory, object_format, head_identity, head_bytes)?;
    unsupported_control_files_are_absent(git_directory.as_fd())?;
    validate_live_shallow(git_directory, object_format)?;
    validate_config_at(git_directory, config_snapshot, config_identity)?;
    // Repeat the mutable-file checks to bracket config validation and catch a
    // concurrent change that occurs between either side of the sequence.
    validate_head_at(worktree_directory, object_format, head_identity, head_bytes)?;
    validate_live_shallow(git_directory, object_format)?;
    unsupported_control_files_are_absent(git_directory.as_fd())?;
    validate_head_at(worktree_directory, object_format, head_identity, head_bytes)?;
    validate_directory_binding(git_directory, OsStr::new("refs"), refs)?;
    crate::repository_directories::validate_binding(root, administration)?;
    let administrative_directory = dup(git_directory).map_err(|_| LocalGitFailure::Repository)?;
    reject_administrative_symlinks(&administrative_directory, object_format)
        .map_err(|_| LocalGitFailure::Repository)?;
    if administration.worktree != administration.common {
        let worktree = dup(worktree_directory).map_err(|_| LocalGitFailure::Repository)?;
        reject_administrative_symlinks(&worktree, object_format)
            .map_err(|_| LocalGitFailure::Repository)?;
    }
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
    bind_still_holds(parent.as_fd(), name, pinned, || LocalGitFailure::Repository)
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
        let source = ObjectSource::open(
            authority,
            std::time::Instant::now() + crate::push_executor::PUSH_PREPARATION_TIMEOUT,
        )?;
        let pack = openat(
            &authority.git_directory,
            "objects/pack",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        Ok(Self {
            source: Arc::new(Mutex::new(source)),
            pack,
        })
    }

    pub(super) fn add_to(&self, database: &Odb<'_>) -> Result<(), LocalGitFailure> {
        self.source
            .lock()
            .map_err(|_| LocalGitFailure::Operation)?
            .attach(database)
    }

    pub(super) fn pack_directory(&self) -> &OwnedFd {
        &self.pack
    }

    pub(super) fn contains(&self, oid: git2::Oid) -> Result<bool, LocalGitFailure> {
        self.source
            .lock()
            .map_err(|_| LocalGitFailure::Operation)?
            .contains(oid)
    }

    pub(super) fn validate_live(
        &self,
        authority: &PinnedRepository,
    ) -> Result<(), LocalGitFailure> {
        self.source
            .lock()
            .map_err(|_| LocalGitFailure::Operation)?
            .validate(authority)
    }

    #[cfg(test)]
    pub(super) fn capture_with_test_hook<Hook: FnOnce()>(
        authority: &PinnedRepository,
        hook: Hook,
    ) -> Result<Self, LocalGitFailure> {
        let snapshot = Self::capture(authority)?;
        hook();
        snapshot.validate_live(authority)?;
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(super) fn capture_with_post_bindings_test_hook<Hook: FnOnce()>(
        authority: &PinnedRepository,
        hook: Hook,
    ) -> Result<Self, LocalGitFailure> {
        Self::capture_with_test_hook(authority, hook)
    }
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
        max_object_bytes: None,
        repository,
        _directory: directory,
        selected_objects: RefCell::new(None),
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
