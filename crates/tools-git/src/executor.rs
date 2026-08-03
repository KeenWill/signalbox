use std::{
    cell::RefCell,
    collections::BTreeSet,
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use git2::{
    CheckoutNotificationType, Index, IndexEntry, IndexTime, Mempack, Odb, Repository,
    build::CheckoutBuilder,
};
use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_domain::ToolExecutionErrorDetail;
#[cfg(test)]
use signalbox_tools_workspace::LocalWorkspaceFileSystem;
use signalbox_tools_workspace::{
    WorkspaceEntryKind, WorkspaceFileSystem, WorkspaceResolveError, WorkspaceRoot,
};

use crate::arguments::{
    GitBranchSwitchArguments, GitDiffArguments, GitStageArguments, LocalOperation,
    checked_relative_path, parse_gitdir_marker,
};
use crate::bounded::{
    find_bounded_commit, find_bounded_tree, tree_for_commit, validate_checkout_tree_discovery,
    validate_index_entry_count, validate_index_objects,
};
use crate::branch::branch_create;
use crate::commit::{RepositoryOperationState, commit, publish_symbolic_head};
use crate::descriptor::{RepositoryIdentity, descriptor_path};
use crate::diff::diff;
use crate::failure::LocalGitFailure;
use crate::identity::GitIdentity;
use crate::index_lock::{IndexLock, IndexSnapshot};
use crate::layout::{valid_reference_name, validate_repository_layout};
use crate::limits::{
    GITLINK_MODE, INDEX_SKIP_WORKTREE, MAX_REFERENCE_BYTES, MAX_REVISION_BYTES,
    MAX_STAGE_FILE_BYTES, MAX_STAGE_TOTAL_BYTES, MAX_WORKTREE_INSPECTIONS, MAX_WORKTREE_PATH_BYTES,
};
use crate::log::log;
use crate::objects::{PackRoot, persist_objects};
use crate::pinning::{
    PinnedObjectDatabase, PinnedRepository, repository_filemode, repository_ignorecase,
};
use crate::reference_lock::ReferenceLock;
use crate::reference_read::resolve_pinned_reference_chain_from;
use crate::result::{BranchResult, LocalGitResult, StageResult, encode_result};
use crate::rollback::{
    CheckoutRollbackContext, WorktreeRollbackIdentities, capture_rollback_identities,
    capture_rollback_identity, capture_worktree_rollback_state, checkout_tree_with_rollback,
    restore_index, rollback_checkout_atomically, validate_checkout_path,
};
use crate::status::{status, tracked_directories};

/// Executor for local Git operations only.
#[derive(Debug)]
pub struct LocalGitExecutor<FileSystem> {
    pub(super) filesystem: FileSystem,
    pub(super) root: WorkspaceRoot,
    root_path: PathBuf,
    repository_identity: RepositoryIdentity,
    pub(super) repository_authority: PinnedRepository,
    pub(super) identity: GitIdentity,
    pub(super) repository_detail: ToolExecutionErrorDetail,
    pub(super) path_detail: ToolExecutionErrorDetail,
    pub(super) operation_detail: ToolExecutionErrorDetail,
}

/// Sanitized local Git executor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalGitExecutorError;

impl fmt::Display for LocalGitExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("local Git executor contract failed")
    }
}

impl Error for LocalGitExecutorError {}

impl ClassifyOperatorFailure for LocalGitExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

impl<FileSystem: WorkspaceFileSystem> LocalGitExecutor<FileSystem> {
    pub(super) fn execute_operation(
        &self,
        operation: LocalOperation,
    ) -> Result<String, LocalGitFailure> {
        self.execute_operation_with_hooks(operation, || {}, || {})
    }

    #[cfg(test)]
    pub(super) fn execute_read_with_return_hook<Hook: FnOnce()>(
        &self,
        operation: LocalOperation,
        before_read_return: Hook,
    ) -> Result<String, LocalGitFailure> {
        self.execute_operation_with_hooks(operation, before_read_return, || {})
    }

    #[cfg(test)]
    pub(super) fn execute_commit_with_publish_hook<Hook: FnOnce()>(
        &self,
        operation: LocalOperation,
        before_commit_publish: Hook,
    ) -> Result<String, LocalGitFailure> {
        self.execute_operation_with_hooks(operation, || {}, before_commit_publish)
    }

    fn execute_operation_with_hooks<ReadHook: FnOnce(), CommitHook: FnOnce()>(
        &self,
        operation: LocalOperation,
        before_read_return: ReadHook,
        before_commit_publish: CommitHook,
    ) -> Result<String, LocalGitFailure> {
        self.validate_current_repository()?;
        let mut repository = self.repository_authority.repository()?;
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)?;
        let persistent_object_database = Odb::new_ext(self.repository_authority.object_format)
            .map_err(|_| LocalGitFailure::Operation)?;
        pinned_objects.add_to(&persistent_object_database)?;
        let object_database = Odb::new_ext(self.repository_authority.object_format)
            .map_err(|_| LocalGitFailure::Operation)?;
        pinned_objects.add_to(&object_database)?;
        let mempack = object_database
            .add_new_mempack_backend(1000)
            .map_err(|_| LocalGitFailure::Operation)?;
        repository
            .set_odb(&object_database)
            .map_err(|_| LocalGitFailure::Operation)?;
        let revalidate_captured_objects = matches!(
            &operation,
            LocalOperation::Status | LocalOperation::Diff(_) | LocalOperation::Log(_)
        );
        let result = match operation {
            LocalOperation::Status => {
                let index_snapshot = self.bind_index_snapshot(&repository)?;
                let untracked = self.discover_untracked_paths(&repository)?;
                let status = status(
                    &repository,
                    &self.repository_authority,
                    &self.filesystem,
                    &self.root,
                    untracked,
                )?;
                index_snapshot.validate()?;
                LocalGitResult::Status(status)
            }
            LocalOperation::Diff(arguments) => {
                let index_snapshot = if matches!(arguments, GitDiffArguments::Worktree) {
                    Some(self.bind_index_snapshot(&repository)?)
                } else {
                    None
                };
                let untracked = if index_snapshot.is_some() {
                    self.discover_untracked_paths(&repository)?
                } else {
                    Vec::new()
                };
                let diff = diff(
                    &repository,
                    &self.repository_authority,
                    arguments,
                    &self.filesystem,
                    &self.root,
                    untracked,
                )?;
                if let Some(index_snapshot) = index_snapshot {
                    index_snapshot.validate()?;
                }
                LocalGitResult::Diff(diff)
            }
            LocalOperation::Log(arguments) => {
                LocalGitResult::Log(log(&repository, &self.repository_authority, arguments)?)
            }
            LocalOperation::Stage(arguments) => {
                let result = LocalGitResult::Stage(self.stage_with_pinned_objects(
                    &repository,
                    (
                        &persistent_object_database,
                        &object_database,
                        &mempack,
                        &pinned_objects,
                    ),
                    arguments,
                    || {},
                )?);
                return encode_result(&result);
            }
            LocalOperation::Commit(arguments) => {
                let result = LocalGitResult::Commit(commit(
                    &mut repository,
                    &self.identity,
                    arguments,
                    &self.repository_authority,
                    (
                        &persistent_object_database,
                        &object_database,
                        &pinned_objects,
                    ),
                    || {
                        before_commit_publish();
                        pinned_objects.validate_live(&self.repository_authority)?;
                        self.validate_current_repository_identity()
                    },
                )?);
                return encode_result(&result);
            }
            LocalOperation::BranchCreate(arguments) => {
                let result = LocalGitResult::BranchCreate(branch_create(
                    &repository,
                    &self.repository_authority,
                    &object_database,
                    &pinned_objects,
                    arguments,
                    || {
                        pinned_objects.validate_live(&self.repository_authority)?;
                        self.validate_current_repository()
                    },
                )?);
                return encode_result(&result);
            }
            LocalOperation::BranchSwitch(arguments) => {
                let result = LocalGitResult::BranchSwitch(self.branch_switch_with_pinned_objects(
                    &repository,
                    &pinned_objects,
                    arguments,
                )?);
                return encode_result(&result);
            }
        };
        if revalidate_captured_objects {
            before_read_return();
            pinned_objects.validate_live(&self.repository_authority)?;
        }
        self.validate_current_repository()?;
        encode_result(&result)
    }

    #[cfg(test)]
    pub(super) fn stage(
        &self,
        repository: &Repository,
        arguments: GitStageArguments,
    ) -> Result<StageResult, LocalGitFailure> {
        self.stage_with_pre_publish_hook(repository, arguments, || {})
    }

    #[cfg(test)]
    pub(super) fn stage_with_pre_publish_hook<BeforePublish>(
        &self,
        repository: &Repository,
        arguments: GitStageArguments,
        before_publish: BeforePublish,
    ) -> Result<StageResult, LocalGitFailure>
    where
        BeforePublish: FnOnce(),
    {
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)?;
        let persistent_object_database = Odb::new_ext(self.repository_authority.object_format)
            .map_err(|_| LocalGitFailure::Operation)?;
        pinned_objects.add_to(&persistent_object_database)?;
        let object_database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
        let mempack = object_database
            .add_new_mempack_backend(1000)
            .map_err(|_| LocalGitFailure::Operation)?;
        self.stage_with_pinned_objects(
            repository,
            (
                &persistent_object_database,
                &object_database,
                &mempack,
                &pinned_objects,
            ),
            arguments,
            before_publish,
        )
    }

    fn stage_with_pinned_objects<BeforePublish>(
        &self,
        repository: &Repository,
        object_databases: (&Odb<'_>, &Odb<'_>, &Mempack<'_>, &PinnedObjectDatabase),
        arguments: GitStageArguments,
        before_publish: BeforePublish,
    ) -> Result<StageResult, LocalGitFailure>
    where
        BeforePublish: FnOnce(),
    {
        let (persistent_object_database, object_database, _mempack, pinned_objects) =
            object_databases;
        let (mut index_lock, mut index) =
            IndexLock::acquire_for_repository(&self.repository_authority)?;
        validate_index_entry_count(&index)?;
        let filemode = repository_filemode(repository)?;
        let mut planned = Vec::with_capacity(arguments.paths.len());
        let mut total_bytes = 0_usize;
        for supplied in &arguments.paths {
            let path = checked_relative_path(supplied).map_err(|_| LocalGitFailure::Path)?;
            match self
                .filesystem
                .read_file_prefix(&self.root, &path, MAX_STAGE_FILE_BYTES)
            {
                Ok(read) if read.truncated => return Err(LocalGitFailure::Operation),
                Ok(read) => {
                    total_bytes = total_bytes
                        .checked_add(read.bytes.len())
                        .filter(|total| *total <= MAX_STAGE_TOTAL_BYTES)
                        .ok_or(LocalGitFailure::Operation)?;
                    let observed_mode = if read.mode & 0o111 == 0 {
                        0o100644
                    } else {
                        0o100755
                    };
                    let indexed_mode = index.get_path(&path, 0).map(|entry| entry.mode);
                    let mode = regular_file_mode(observed_mode, indexed_mode, filemode);
                    planned.push(PlannedStage::Add {
                        path,
                        bytes: read.bytes,
                        mode,
                    });
                }
                Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
                Err(WorkspaceResolveError::Io { source, .. })
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    if let Some(indexed) = index.get_path(&path, 0) {
                        if indexed.mode == GITLINK_MODE {
                            return Err(LocalGitFailure::Operation);
                        }
                        planned.push(PlannedStage::Remove { path });
                    } else if index_path_is_conflicted(&index, &path) {
                        planned.push(PlannedStage::RemoveConflict { path });
                    } else {
                        return Err(LocalGitFailure::Operation);
                    }
                }
                Err(WorkspaceResolveError::Io { .. }) if index.get_path(&path, 0).is_some() => {
                    let indexed = index.get_path(&path, 0).ok_or(LocalGitFailure::Operation)?;
                    match self.filesystem.entry_kind(&self.root, &path) {
                        Ok(WorkspaceEntryKind::Directory) if indexed.mode != GITLINK_MODE => {
                            planned.push(PlannedStage::Remove { path });
                        }
                        Ok(WorkspaceEntryKind::Directory) => {
                            return Err(LocalGitFailure::Operation);
                        }
                        Ok(WorkspaceEntryKind::Symlink | WorkspaceEntryKind::Other)
                        | Err(WorkspaceResolveError::Rejected(_)) => {
                            return Err(LocalGitFailure::Path);
                        }
                        Ok(WorkspaceEntryKind::File) | Err(WorkspaceResolveError::Io { .. }) => {
                            return Err(LocalGitFailure::Operation);
                        }
                    }
                }
                Err(WorkspaceResolveError::Io { .. }) => return Err(LocalGitFailure::Operation),
            }
        }
        self.validate_current_repository()?;
        let mut written_objects = Vec::new();
        for operation in planned {
            match operation {
                PlannedStage::Add { path, bytes, mode } => {
                    // Path-aware blob writers reopen attribute files; insert
                    // the already-bounded descriptor bytes without a second
                    // model-writable pathname lookup.
                    let oid = repository
                        .blob(&bytes)
                        .map_err(|_| LocalGitFailure::Operation)?;
                    written_objects.push(PackRoot::Object(oid));
                    let entry = IndexEntry {
                        ctime: IndexTime::new(0, 0),
                        mtime: IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode,
                        uid: 0,
                        gid: 0,
                        file_size: u32::try_from(bytes.len())
                            .map_err(|_| LocalGitFailure::Operation)?,
                        id: oid,
                        flags: 0,
                        flags_extended: 0,
                        path: path.as_os_str().as_bytes().to_vec(),
                    };
                    if index_path_is_conflicted(&index, &path) {
                        index
                            .conflict_remove(&path)
                            .map_err(|_| LocalGitFailure::Operation)?;
                    }
                    index.add(&entry).map_err(|_| LocalGitFailure::Operation)?;
                }
                PlannedStage::Remove { path } => index
                    .remove_path(&path)
                    .map_err(|_| LocalGitFailure::Operation)?,
                PlannedStage::RemoveConflict { path } => index
                    .conflict_remove(&path)
                    .map_err(|_| LocalGitFailure::Operation)?,
            }
        }
        validate_index_objects(repository, &index)?;
        persist_objects(
            &self.repository_authority,
            repository,
            persistent_object_database,
            object_database,
            pinned_objects,
            &written_objects,
        )?;
        index_lock.write(&mut index)?;
        before_publish();
        pinned_objects.validate_live(&self.repository_authority)?;
        self.validate_live_index_objects(&index)?;
        drop(index);
        self.validate_current_repository_identity()?;
        index_lock.commit()?;
        self.validate_current_repository()?;
        Ok(StageResult {
            staged_paths: arguments.paths.len(),
        })
    }

    pub(super) fn validate_current_repository(&self) -> Result<(), LocalGitFailure> {
        self.repository_authority.validate_supported_layout()?;
        let observed = validate_repository_layout(&self.root_path, self.root.identity())
            .map_err(|_| LocalGitFailure::Repository)?;
        // HEAD is operation state, not repository identity. A completed branch
        // switch or detached commit establishes the next operation's baseline;
        // each operation separately snapshots and revalidates the references it
        // reads before returning.
        if observed.root == self.repository_identity.root
            && observed.git_directory == self.repository_identity.git_directory
            && observed.refs == self.repository_identity.refs
            && observed.config == self.repository_identity.config
        {
            Ok(())
        } else {
            Err(LocalGitFailure::Repository)
        }
    }

    pub(super) fn validate_current_repository_identity(&self) -> Result<(), LocalGitFailure> {
        self.validate_current_repository()
    }

    fn validate_live_index_objects(&self, index: &Index) -> Result<(), LocalGitFailure> {
        let repository = self.repository_authority.open_repository_shell()?;
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)?;
        let object_database = Odb::new_ext(self.repository_authority.object_format)
            .map_err(|_| LocalGitFailure::Operation)?;
        pinned_objects.add_to(&object_database)?;
        repository
            .set_odb(&object_database)
            .map_err(|_| LocalGitFailure::Operation)?;
        validate_index_objects(&repository, index)?;
        pinned_objects.validate_live(&self.repository_authority)
    }

    pub(super) fn bind_locked_index(
        &self,
        repository: &Repository,
    ) -> Result<IndexLock, LocalGitFailure> {
        let (index_lock, mut index) =
            IndexLock::acquire_for_repository(&self.repository_authority)?;
        repository
            .set_index(&mut index)
            .map_err(|_| LocalGitFailure::Operation)?;
        Ok(index_lock)
    }

    fn bind_index_snapshot(
        &self,
        repository: &Repository,
    ) -> Result<IndexSnapshot, LocalGitFailure> {
        let (snapshot, mut index) =
            IndexSnapshot::acquire_for_repository(&self.repository_authority)?;
        repository
            .set_index(&mut index)
            .map_err(|_| LocalGitFailure::Operation)?;
        Ok(snapshot)
    }

    fn discover_untracked_paths(
        &self,
        repository: &Repository,
    ) -> Result<Vec<PathBuf>, LocalGitFailure> {
        let index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
        if index.len() > MAX_WORKTREE_INSPECTIONS {
            return Err(LocalGitFailure::Operation);
        }
        let tracked_directories = tracked_directories(&index);
        let tracked_paths = index
            .iter()
            .map(|entry| PathBuf::from(OsString::from_vec(entry.path)))
            .collect::<BTreeSet<_>>();
        let ignorecase = repository_ignorecase(repository)?;
        let tracked_directory_keys = ignorecase.then(|| {
            tracked_directories
                .iter()
                .map(|path| ignorecase_path(path))
                .collect::<BTreeSet<_>>()
        });
        let tracked_path_keys = ignorecase.then(|| {
            tracked_paths
                .iter()
                .map(|path| ignorecase_path(path))
                .collect::<BTreeSet<_>>()
        });
        let mut pending = vec![PathBuf::from(".")];
        let mut untracked = Vec::new();
        let mut inspected = 0_usize;
        let mut inspected_path_bytes = 0_usize;
        while let Some(directory) = pending.pop() {
            let directory_is_tracked = tracked_directory_keys.as_ref().map_or_else(
                || tracked_directories.contains(&directory),
                |keys| keys.contains(&ignorecase_path(&directory)),
            );
            if !directory_is_tracked && self.is_embedded_repository(&directory)? {
                untracked.push(directory);
                continue;
            }
            let remaining_entries = MAX_WORKTREE_INSPECTIONS.saturating_sub(inspected);
            let remaining_path_bytes = MAX_WORKTREE_PATH_BYTES.saturating_sub(inspected_path_bytes);
            let requested_entries = remaining_entries.saturating_add(1);
            let read = self
                .filesystem
                .read_directory(
                    &self.root,
                    &directory,
                    requested_entries,
                    requested_entries,
                    remaining_path_bytes,
                )
                .map_err(|error| match error {
                    WorkspaceResolveError::Rejected(_) => LocalGitFailure::Path,
                    WorkspaceResolveError::Io { .. } => LocalGitFailure::Operation,
                })?;
            if read.truncated
                || read.inspected_entries > remaining_entries
                || read.inspected_path_bytes > remaining_path_bytes
            {
                return Err(LocalGitFailure::Operation);
            }
            inspected = inspected.saturating_add(read.inspected_entries);
            inspected_path_bytes = inspected_path_bytes.saturating_add(read.inspected_path_bytes);
            for entry in read.entries {
                if entry.path == Path::new(".git")
                    || entry.path.file_name() == Some(OsStr::new(".git"))
                        && self.is_embedded_repository(
                            entry.path.parent().ok_or(LocalGitFailure::Path)?,
                        )?
                {
                    continue;
                }
                match entry.kind {
                    WorkspaceEntryKind::Directory => {
                        if index
                            .get_path(&entry.path, 0)
                            .is_none_or(|indexed| indexed.mode != GITLINK_MODE)
                        {
                            pending.push(entry.path);
                        }
                    }
                    WorkspaceEntryKind::File
                    | WorkspaceEntryKind::Symlink
                    | WorkspaceEntryKind::Other => {
                        let path_is_tracked = tracked_path_keys.as_ref().map_or_else(
                            || tracked_paths.contains(&entry.path),
                            |keys| keys.contains(&ignorecase_path(&entry.path)),
                        );
                        if !path_is_tracked {
                            untracked.push(entry.path);
                        }
                    }
                }
            }
        }
        Ok(untracked)
    }

    fn is_embedded_repository(&self, directory: &Path) -> Result<bool, LocalGitFailure> {
        if directory == Path::new(".") {
            return Ok(false);
        }
        let dot_git = directory.join(".git");
        match self.filesystem.entry_kind(&self.root, &dot_git) {
            Ok(WorkspaceEntryKind::Directory) => self.is_repository_directory(&dot_git),
            Ok(WorkspaceEntryKind::File) => {
                let marker = self
                    .filesystem
                    .read_file_prefix(&self.root, &dot_git, MAX_REVISION_BYTES)
                    .map_err(|error| match error {
                        WorkspaceResolveError::Rejected(_) => LocalGitFailure::Path,
                        WorkspaceResolveError::Io { .. } => LocalGitFailure::Operation,
                    })?;
                if marker.truncated {
                    return Ok(false);
                }
                let target = parse_gitdir_marker(directory, &marker.bytes);
                target.map_or(Ok(false), |target| self.is_repository_directory(&target))
            }
            Ok(WorkspaceEntryKind::Symlink | WorkspaceEntryKind::Other) => Ok(false),
            Err(WorkspaceResolveError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(false)
            }
            Err(WorkspaceResolveError::Rejected(_)) => Err(LocalGitFailure::Path),
            Err(WorkspaceResolveError::Io { .. }) => Err(LocalGitFailure::Operation),
        }
    }

    fn is_repository_directory(&self, directory: &Path) -> Result<bool, LocalGitFailure> {
        let head = self
            .filesystem
            .entry_kind(&self.root, &directory.join("HEAD"));
        let objects = self
            .filesystem
            .entry_kind(&self.root, &directory.join("objects"));
        match (head, objects) {
            (Ok(WorkspaceEntryKind::File), Ok(WorkspaceEntryKind::Directory)) => Ok(true),
            (Err(WorkspaceResolveError::Rejected(_)), _)
            | (_, Err(WorkspaceResolveError::Rejected(_))) => Err(LocalGitFailure::Path),
            _ => Ok(false),
        }
    }

    pub(super) fn branch_switch(
        &self,
        repository: &Repository,
        arguments: GitBranchSwitchArguments,
    ) -> Result<BranchResult, LocalGitFailure> {
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)?;
        self.branch_switch_with_hooks(
            repository,
            &pinned_objects,
            arguments,
            (|| {}, || {}, || {}, || {}),
        )
    }

    fn branch_switch_with_pinned_objects(
        &self,
        repository: &Repository,
        pinned_objects: &PinnedObjectDatabase,
        arguments: GitBranchSwitchArguments,
    ) -> Result<BranchResult, LocalGitFailure> {
        self.branch_switch_with_hooks(
            repository,
            pinned_objects,
            arguments,
            (|| {}, || {}, || {}, || {}),
        )
    }

    #[cfg(test)]
    pub(super) fn branch_switch_with_hook<Hook: FnOnce()>(
        &self,
        arguments: GitBranchSwitchArguments,
        post_checkout: Hook,
    ) -> Result<BranchResult, LocalGitFailure> {
        self.validate_current_repository()?;
        let repository = self.repository_authority.repository()?;
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)?;
        let object_database = Odb::new_ext(self.repository_authority.object_format)
            .map_err(|_| LocalGitFailure::Operation)?;
        pinned_objects.add_to(&object_database)?;
        repository
            .set_odb(&object_database)
            .map_err(|_| LocalGitFailure::Operation)?;
        self.branch_switch_with_hooks(
            &repository,
            &pinned_objects,
            arguments,
            (|| {}, post_checkout, || {}, || {}),
        )
    }

    #[cfg(test)]
    pub(super) fn branch_switch_with_reference_lock_hook<Hook: FnOnce()>(
        &self,
        repository: &Repository,
        arguments: GitBranchSwitchArguments,
        before_reference_locks: Hook,
    ) -> Result<BranchResult, LocalGitFailure> {
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)?;
        self.branch_switch_with_hooks(
            repository,
            &pinned_objects,
            arguments,
            (before_reference_locks, || {}, || {}, || {}),
        )
    }

    #[cfg(test)]
    pub(super) fn branch_switch_with_index_publish_hook<Hook: FnOnce()>(
        &self,
        repository: &Repository,
        arguments: GitBranchSwitchArguments,
        post_index_publish: Hook,
    ) -> Result<BranchResult, LocalGitFailure> {
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)?;
        self.branch_switch_with_hooks(
            repository,
            &pinned_objects,
            arguments,
            (|| {}, || {}, post_index_publish, || {}),
        )
    }

    #[cfg(test)]
    pub(super) fn branch_switch_with_head_publish_hook<Hook: FnOnce()>(
        &self,
        arguments: GitBranchSwitchArguments,
        before_head_publish: Hook,
    ) -> Result<BranchResult, LocalGitFailure> {
        self.validate_current_repository()?;
        let repository = self.repository_authority.repository()?;
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)?;
        let object_database = Odb::new_ext(self.repository_authority.object_format)
            .map_err(|_| LocalGitFailure::Operation)?;
        pinned_objects.add_to(&object_database)?;
        repository
            .set_odb(&object_database)
            .map_err(|_| LocalGitFailure::Operation)?;
        self.branch_switch_with_hooks(
            &repository,
            &pinned_objects,
            arguments,
            (|| {}, || {}, || {}, before_head_publish),
        )
    }

    fn branch_switch_with_hooks<
        BeforeLocks: FnOnce(),
        PostCheckout: FnOnce(),
        PostIndexPublish: FnOnce(),
        BeforeHeadPublish: FnOnce(),
    >(
        &self,
        repository: &Repository,
        pinned_objects: &PinnedObjectDatabase,
        arguments: GitBranchSwitchArguments,
        hooks: (
            BeforeLocks,
            PostCheckout,
            PostIndexPublish,
            BeforeHeadPublish,
        ),
    ) -> Result<BranchResult, LocalGitFailure> {
        let (before_reference_locks, post_checkout, post_index_publish, before_head_publish) =
            hooks;
        let mut index_lock = self.bind_locked_index(repository)?;
        let operation_state = RepositoryOperationState::capture(&self.repository_authority)?;
        operation_state.require_clean()?;
        let reference_name = format!("refs/heads/{}", arguments.name);
        if reference_name.len() > MAX_REFERENCE_BYTES
            || !valid_reference_name(reference_name.as_bytes())
        {
            return Err(LocalGitFailure::Operation);
        }
        let (current_chain, initial_current) =
            resolve_pinned_reference_chain_from(&self.repository_authority, "HEAD", None)?;
        let (reference_chain, initial_target) =
            resolve_pinned_reference_chain_from(&self.repository_authority, &reference_name, None)?;
        let initial_target = initial_target.ok_or(LocalGitFailure::Operation)?;
        before_reference_locks();
        let lock_names = current_chain
            .iter()
            .chain(reference_chain.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut reference_locks = lock_names
            .iter()
            .map(|reference| ReferenceLock::acquire(&self.repository_authority, reference))
            .collect::<Result<Vec<_>, _>>()?;
        let (locked_current_chain, current_target) = resolve_pinned_reference_chain_from(
            &self.repository_authority,
            "HEAD",
            Some(&reference_locks),
        )?;
        let (locked_chain, target) = resolve_pinned_reference_chain_from(
            &self.repository_authority,
            &reference_name,
            Some(&reference_locks),
        )?;
        if locked_current_chain != current_chain
            || current_target != initial_current
            || locked_chain != reference_chain
            || target != Some(initial_target)
        {
            return Err(LocalGitFailure::Operation);
        }
        let signature = self
            .identity
            .signature()
            .map_err(|_| LocalGitFailure::Operation)?;
        let target = initial_target;
        let target_commit = find_bounded_commit(repository, target)?;
        let current_tree = current_target
            .map(|current| tree_for_commit(repository, current))
            .transpose()?;
        let target_tree = find_bounded_tree(repository, target_commit.tree_id())?;
        if let Some(current_tree) = &current_tree {
            validate_checkout_tree_discovery(repository, current_tree)?;
        }
        validate_checkout_tree_discovery(repository, &target_tree)?;
        let current_index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
        if current_index.has_conflicts() {
            return Err(LocalGitFailure::Operation);
        }
        validate_index_objects(repository, &current_index)?;
        if current_index.iter().any(|entry| {
            entry.flags & 0x3000 == 0 && entry.flags_extended & INDEX_SKIP_WORKTREE != 0
        }) {
            return Err(LocalGitFailure::Operation);
        }
        let staged = repository
            .diff_tree_to_index(current_tree.as_ref(), Some(&current_index), None)
            .map_err(|_| LocalGitFailure::Operation)?;
        let staged_paths = staged
            .deltas()
            .flat_map(|delta| {
                [delta.old_file().path(), delta.new_file().path()]
                    .into_iter()
                    .flatten()
                    .map(Path::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>();
        let changes = repository
            .diff_tree_to_tree(current_tree.as_ref(), Some(&target_tree), None)
            .map_err(|_| LocalGitFailure::Operation)?;
        let checkout_paths = changes
            .deltas()
            .filter_map(|delta| {
                delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .map(Path::to_owned)
            })
            .collect::<BTreeSet<_>>();
        if !staged_paths.is_disjoint(&checkout_paths) {
            return Err(LocalGitFailure::Operation);
        }
        let staged_entries = staged_paths
            .into_iter()
            .map(|path| {
                let entry = current_index
                    .get_path(&path, 0)
                    .map(|entry| clone_index_entry(&entry));
                (path, entry)
            })
            .collect::<Vec<_>>();
        for path in &checkout_paths {
            validate_checkout_path(
                &self.filesystem,
                &self.root,
                path,
                &current_index,
                &target_tree,
            )?;
        }
        let mut next_index = Index::new_ext(self.repository_authority.object_format)
            .map_err(|_| LocalGitFailure::Operation)?;
        next_index
            .read_tree(&target_tree)
            .map_err(|_| LocalGitFailure::Operation)?;
        for current_entry in current_index
            .iter()
            .filter(|entry| entry.flags & 0x3000 == 0)
        {
            let path = PathBuf::from(OsString::from_vec(current_entry.path.clone()));
            if checkout_paths.contains(&path) {
                if current_entry.flags_extended & INDEX_SKIP_WORKTREE != 0
                    && let Some(target_entry) = next_index.get_path(&path, 0)
                {
                    let mut target_entry = clone_index_entry(&target_entry);
                    target_entry.flags_extended |= INDEX_SKIP_WORKTREE;
                    next_index
                        .add(&target_entry)
                        .map_err(|_| LocalGitFailure::Operation)?;
                }
                continue;
            }
            if let Some(target_entry) = next_index.get_path(&path, 0) {
                let mut target_entry = clone_index_entry(&target_entry);
                target_entry.flags = current_entry.flags;
                target_entry.flags_extended = current_entry.flags_extended;
                next_index
                    .add(&target_entry)
                    .map_err(|_| LocalGitFailure::Operation)?;
            }
        }
        for (path, entry) in staged_entries {
            if let Some(entry) = entry {
                next_index
                    .add(&entry)
                    .map_err(|_| LocalGitFailure::Operation)?;
            } else if next_index.get_path(&path, 0).is_some() {
                next_index
                    .remove_path(&path)
                    .map_err(|_| LocalGitFailure::Operation)?;
            }
        }
        let original_index_bytes = index_lock.original_bytes()?;
        index_lock.write(&mut next_index)?;
        let updated_paths = RefCell::new(BTreeSet::new());
        let updated_identities = RefCell::new(WorktreeRollbackIdentities::new());
        let mut checkout = CheckoutBuilder::new();
        checkout
            .safe()
            .target_dir(&descriptor_path(&self.repository_authority.root))
            .update_index(false)
            .refresh(false)
            .disable_filters(true);
        checkout
            .notify_on(CheckoutNotificationType::UPDATED)
            .notify(|_, path, _, _, _| {
                let paths = if let Some(path) = path {
                    BTreeSet::from([path.to_owned()])
                } else {
                    checkout_paths.clone()
                };
                paths.into_iter().fold(true, |captured, path| {
                    updated_paths.borrow_mut().insert(path.clone());
                    let current = capture_rollback_identity(&self.repository_authority.root, &path)
                        .map(|identity| {
                            updated_identities.borrow_mut().insert(path, identity);
                        })
                        .is_ok();
                    captured && current
                })
            });
        checkout_tree_with_rollback(
            repository,
            current_tree.as_ref(),
            &target_tree,
            &updated_paths,
            &updated_identities,
            CheckoutRollbackContext {
                filesystem: &self.filesystem,
                root: &self.root,
                authority: &self.repository_authority,
            },
            || {
                operation_state.validate(&self.repository_authority)?;
                match current_target {
                    Some(current) => repository
                        .set_head_detached(current)
                        .map_err(|_| LocalGitFailure::Operation)?,
                    None => repository
                        .set_head("refs/heads/signalbox-pinned")
                        .map_err(|_| LocalGitFailure::Operation)?,
                }
                repository
                    .checkout_tree(target_commit.as_object(), Some(&mut checkout))
                    .map_err(|_| LocalGitFailure::Operation)
            },
        )?;
        let before_checkout_capture = checkout_paths
            .iter()
            .map(|path| {
                capture_rollback_identity(&self.repository_authority.root, path)
                    .map(|identity| (path.clone(), identity))
            })
            .collect::<Result<WorktreeRollbackIdentities, _>>();
        let before_checkout_capture = match before_checkout_capture {
            Ok(identities) => identities,
            Err(failure) => {
                rollback_checkout_atomically(
                    repository,
                    current_tree.as_ref(),
                    &target_tree,
                    &checkout_paths,
                    CheckoutRollbackContext {
                        filesystem: &self.filesystem,
                        root: &self.root,
                        authority: &self.repository_authority,
                    },
                    Some(&updated_identities.borrow()),
                )?;
                return Err(failure);
            }
        };
        post_checkout();
        let checkout_state =
            match capture_worktree_rollback_state(&self.filesystem, &self.root, &checkout_paths) {
                Ok(state) => state,
                Err(failure) => {
                    rollback_checkout_atomically(
                        repository,
                        current_tree.as_ref(),
                        &target_tree,
                        &checkout_paths,
                        CheckoutRollbackContext {
                            filesystem: &self.filesystem,
                            root: &self.root,
                            authority: &self.repository_authority,
                        },
                        Some(&before_checkout_capture),
                    )?;
                    return Err(failure);
                }
            };
        let checkout_identities = match capture_rollback_identities(
            &self.repository_authority.root,
            Path::new(""),
            &checkout_state,
        ) {
            Ok(identities) => identities,
            Err(failure) => {
                rollback_checkout_atomically(
                    repository,
                    current_tree.as_ref(),
                    &target_tree,
                    &checkout_paths,
                    CheckoutRollbackContext {
                        filesystem: &self.filesystem,
                        root: &self.root,
                        authority: &self.repository_authority,
                    },
                    Some(&before_checkout_capture),
                )?;
                return Err(failure);
            }
        };
        if checkout_identities != before_checkout_capture {
            rollback_checkout_atomically(
                repository,
                current_tree.as_ref(),
                &target_tree,
                &checkout_paths,
                CheckoutRollbackContext {
                    filesystem: &self.filesystem,
                    root: &self.root,
                    authority: &self.repository_authority,
                },
                Some(&before_checkout_capture),
            )?;
            return Err(LocalGitFailure::Operation);
        }
        let published_index = match index_lock.commit() {
            Ok(published_index) => published_index,
            Err(_) => {
                rollback_checkout_atomically(
                    repository,
                    current_tree.as_ref(),
                    &target_tree,
                    &checkout_paths,
                    CheckoutRollbackContext {
                        filesystem: &self.filesystem,
                        root: &self.root,
                        authority: &self.repository_authority,
                    },
                    Some(&checkout_identities),
                )?;
                return Err(LocalGitFailure::Operation);
            }
        };
        post_index_publish();
        let publish_result = (|| {
            let (current_chain_before_publish, current_before_publish) =
                resolve_pinned_reference_chain_from(
                    &self.repository_authority,
                    "HEAD",
                    Some(&reference_locks),
                )?;
            let (target_chain_before_publish, target_before_publish) =
                resolve_pinned_reference_chain_from(
                    &self.repository_authority,
                    &reference_name,
                    Some(&reference_locks),
                )?;
            if current_chain_before_publish != locked_current_chain
                || current_before_publish != current_target
                || target_chain_before_publish != locked_chain
                || target_before_publish != Some(target)
            {
                return Err(LocalGitFailure::Operation);
            }
            let head_lock = reference_locks
                .iter()
                .position(|lock| lock.name == "HEAD")
                .map(|position| reference_locks.swap_remove(position))
                .ok_or(LocalGitFailure::Operation)?;
            self.validate_current_repository()?;
            pinned_objects.validate_live(&self.repository_authority)?;
            operation_state.validate(&self.repository_authority)?;
            publish_symbolic_head(
                &self.repository_authority,
                head_lock,
                &reference_name,
                current_target.unwrap_or(match self.repository_authority.object_format {
                    git2::ObjectFormat::Sha1 => git2::Oid::ZERO_SHA1,
                    git2::ObjectFormat::Sha256 => git2::Oid::ZERO_SHA256,
                }),
                target,
                &signature,
                || {
                    before_head_publish();
                    operation_state.validate(&self.repository_authority)?;
                    published_index.validate()?;
                    let (target_chain, target_now) = resolve_pinned_reference_chain_from(
                        &self.repository_authority,
                        &reference_name,
                        Some(&reference_locks),
                    )?;
                    if target_chain != locked_chain || target_now != Some(target) {
                        return Err(LocalGitFailure::Operation);
                    }
                    self.validate_current_repository_identity()
                },
            )
        })();
        if let Err(failure) = publish_result {
            let worktree_rollback = rollback_checkout_atomically(
                repository,
                current_tree.as_ref(),
                &target_tree,
                &checkout_paths,
                CheckoutRollbackContext {
                    filesystem: &self.filesystem,
                    root: &self.root,
                    authority: &self.repository_authority,
                },
                Some(&checkout_identities),
            );
            let index_rollback = restore_index(
                &self.repository_authority,
                &original_index_bytes,
                published_index.file_identity(),
            );
            worktree_rollback?;
            index_rollback?;
            return Err(failure);
        }
        Ok(BranchResult {
            branch: arguments.name,
            head: target.to_string(),
        })
    }
}

#[cfg(test)]
impl LocalGitExecutor<LocalWorkspaceFileSystem> {
    pub(super) fn for_test(root_path: &Path, identity: GitIdentity) -> Self {
        let filesystem = LocalWorkspaceFileSystem;
        let root =
            WorkspaceRoot::try_new(&filesystem, root_path).expect("fixture workspace root pins");
        let root_path = std::fs::canonicalize(root_path).expect("fixture root canonicalizes");
        let repository_identity = validate_repository_layout(&root_path, root.identity())
            .expect("fixture repository layout validates");
        let repository_authority = PinnedRepository::open(&root_path, repository_identity)
            .expect("fixture repository authority pins");
        let detail = || {
            ToolExecutionErrorDetail::try_new("fixture Git operation failed".to_owned())
                .expect("fixture detail constructs")
        };
        Self {
            filesystem,
            root,
            root_path,
            repository_identity,
            repository_authority,
            identity,
            repository_detail: detail(),
            path_detail: detail(),
            operation_detail: detail(),
        }
    }
}

pub(super) enum PlannedStage {
    Add {
        path: PathBuf,
        bytes: Vec<u8>,
        mode: u32,
    },
    Remove {
        path: PathBuf,
    },
    RemoveConflict {
        path: PathBuf,
    },
}

pub(super) fn regular_file_mode(observed: u32, indexed: Option<u32>, filemode: bool) -> u32 {
    if filemode {
        return observed;
    }
    match indexed {
        Some(0o100644) => 0o100644,
        Some(0o100755) => 0o100755,
        Some(_) => observed,
        None => 0o100644,
    }
}

fn ignorecase_path(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_ascii_lowercase()
}

pub(super) fn clone_index_entry(entry: &IndexEntry) -> IndexEntry {
    IndexEntry {
        ctime: entry.ctime,
        mtime: entry.mtime,
        dev: entry.dev,
        ino: entry.ino,
        mode: entry.mode,
        uid: entry.uid,
        gid: entry.gid,
        file_size: entry.file_size,
        id: entry.id,
        flags: entry.flags,
        flags_extended: entry.flags_extended,
        path: entry.path.clone(),
    }
}

pub(super) fn index_path_is_conflicted(index: &git2::Index, path: &Path) -> bool {
    index.get_path(path, 1).is_some()
        || index.get_path(path, 2).is_some()
        || index.get_path(path, 3).is_some()
}
