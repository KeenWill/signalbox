use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    os::fd::OwnedFd,
    path::{Component, Path, PathBuf},
};

use git2::{Index, Repository, build::CheckoutBuilder};
use rustix::{
    fs::{AtFlags, Mode, OFlags, RenameFlags, openat, renameat_with, statat, unlinkat},
    io::dup,
};
use signalbox_tools_workspace::{
    WorkspaceEntryKind, WorkspaceFileSystem, WorkspaceResolveError, WorkspaceRoot,
};

use crate::descriptor::{FileIdentity, descriptor_entry_exists, descriptor_path, file_identity};
use crate::failure::LocalGitFailure;
use crate::index_lock::IndexLock;
use crate::limits::{
    MAX_OBJECT_BYTES, MAX_TREE_BLOB_BYTES, MAX_WORKTREE_INSPECTIONS, MAX_WORKTREE_PATH_BYTES,
};
use crate::pinning::PinnedRepository;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum WorktreeRollbackEntry {
    Missing,
    Directory,
    File { bytes: Vec<u8>, mode: u32 },
}

pub(super) type WorktreeRollbackIdentities = BTreeMap<PathBuf, Option<FileIdentity>>;

pub(super) fn capture_worktree_rollback_state<FileSystem: WorkspaceFileSystem>(
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    checkout_paths: &BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, WorktreeRollbackEntry>, LocalGitFailure> {
    let mut pending = checkout_paths.iter().cloned().collect::<Vec<_>>();
    let mut state = BTreeMap::new();
    let mut inspected = 0_usize;
    let mut inspected_path_bytes = 0_usize;
    let mut inspected_file_bytes = 0_usize;
    while let Some(path) = pending.pop() {
        if state.contains_key(&path) {
            continue;
        }
        match filesystem.entry_kind(root, &path) {
            Ok(WorkspaceEntryKind::File) => {
                let read = filesystem
                    .read_file_prefix(root, &path, MAX_OBJECT_BYTES)
                    .map_err(|_| LocalGitFailure::Operation)?;
                inspected_file_bytes = inspected_file_bytes.saturating_add(read.bytes.len());
                if read.truncated
                    || read.total_bytes != read.bytes.len() as u64
                    || inspected_file_bytes > MAX_TREE_BLOB_BYTES
                {
                    return Err(LocalGitFailure::Operation);
                }
                state.insert(
                    path,
                    WorktreeRollbackEntry::File {
                        bytes: read.bytes,
                        mode: read.mode,
                    },
                );
            }
            Ok(WorkspaceEntryKind::Directory) => {
                let remaining_entries = MAX_WORKTREE_INSPECTIONS.saturating_sub(inspected);
                let remaining_path_bytes =
                    MAX_WORKTREE_PATH_BYTES.saturating_sub(inspected_path_bytes);
                let requested_entries = remaining_entries.saturating_add(1);
                let read = filesystem
                    .read_directory(
                        root,
                        &path,
                        requested_entries,
                        requested_entries,
                        remaining_path_bytes,
                    )
                    .map_err(|_| LocalGitFailure::Operation)?;
                if read.truncated
                    || read.inspected_entries > remaining_entries
                    || read.inspected_path_bytes > remaining_path_bytes
                {
                    return Err(LocalGitFailure::Operation);
                }
                inspected = inspected.saturating_add(read.inspected_entries);
                inspected_path_bytes =
                    inspected_path_bytes.saturating_add(read.inspected_path_bytes);
                pending.extend(read.entries.into_iter().map(|entry| entry.path));
                state.insert(path, WorktreeRollbackEntry::Directory);
            }
            Ok(WorkspaceEntryKind::Symlink | WorkspaceEntryKind::Other)
            | Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
            Err(WorkspaceResolveError::Io { source, .. })
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                state.insert(path, WorktreeRollbackEntry::Missing);
            }
            Err(WorkspaceResolveError::Io { .. }) => return Err(LocalGitFailure::Operation),
        }
    }
    Ok(state)
}

pub(super) fn rollback_checkout_atomically<FileSystem: WorkspaceFileSystem>(
    repository: &Repository,
    current_tree: Option<&git2::Tree<'_>>,
    target_tree: &git2::Tree<'_>,
    checkout_paths: &BTreeSet<PathBuf>,
    rollback: CheckoutRollbackContext<'_, FileSystem>,
    expected_identities: Option<&WorktreeRollbackIdentities>,
) -> Result<(), LocalGitFailure> {
    let root_path = descriptor_path(&rollback.authority.root);
    let original = tempfile::Builder::new()
        .prefix(".signalbox-git-original-")
        .tempdir_in(&root_path)
        .map_err(|_| LocalGitFailure::Operation)?;
    let expected = tempfile::Builder::new()
        .prefix(".signalbox-git-expected-")
        .tempdir_in(&root_path)
        .map_err(|_| LocalGitFailure::Operation)?;
    checkout_snapshot(repository, current_tree, checkout_paths, original.path())?;
    checkout_snapshot(
        repository,
        Some(target_tree),
        checkout_paths,
        expected.path(),
    )?;
    let original_prefix = PathBuf::from(
        original
            .path()
            .file_name()
            .ok_or(LocalGitFailure::Operation)?,
    );
    let expected_prefix = PathBuf::from(
        expected
            .path()
            .file_name()
            .ok_or(LocalGitFailure::Operation)?,
    );
    for path in checkout_rollback_roots(checkout_paths) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(original.path().join(parent))
                .map_err(|_| LocalGitFailure::Operation)?;
        }
        let expected_state =
            capture_rollback_subtree(rollback.filesystem, rollback.root, &expected_prefix, &path)?;
        let expected_path_identities = expected_identities.map(|identities| {
            identities
                .iter()
                .filter(|(entry_path, _)| entry_path.starts_with(&path))
                .map(|(entry_path, identity)| (entry_path.clone(), *identity))
                .collect::<WorktreeRollbackIdentities>()
        });
        atomic_restore_checkout_path(
            rollback.filesystem,
            rollback.root,
            rollback.authority,
            &original_prefix,
            &path,
            &expected_state,
            expected_path_identities.as_ref(),
        )?;
    }
    Ok(())
}

pub(super) fn checkout_snapshot(
    repository: &Repository,
    tree: Option<&git2::Tree<'_>>,
    checkout_paths: &BTreeSet<PathBuf>,
    destination: &Path,
) -> Result<(), LocalGitFailure> {
    let Some(tree) = tree else {
        return Ok(());
    };
    let mut checkout = CheckoutBuilder::new();
    checkout
        .force()
        .target_dir(destination)
        .update_index(false)
        .refresh(false)
        .disable_filters(true);
    for path in checkout_paths {
        checkout.path(path);
    }
    repository
        .checkout_tree(tree.as_object(), Some(&mut checkout))
        .map_err(|_| LocalGitFailure::Operation)
}

pub(super) fn checkout_rollback_roots(checkout_paths: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for path in checkout_paths {
        if !roots.iter().any(|root: &PathBuf| path.starts_with(root)) {
            roots.push(path.clone());
        }
    }
    roots
}

pub(super) fn capture_rollback_subtree<FileSystem: WorkspaceFileSystem>(
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    prefix: &Path,
    path: &Path,
) -> Result<BTreeMap<PathBuf, WorktreeRollbackEntry>, LocalGitFailure> {
    let full_path = prefix.join(path);
    let state = capture_worktree_rollback_state(filesystem, root, &BTreeSet::from([full_path]))?;
    state
        .into_iter()
        .map(|(entry_path, entry)| {
            entry_path
                .strip_prefix(prefix)
                .map(Path::to_owned)
                .map(|entry_path| (entry_path, entry))
                .map_err(|_| LocalGitFailure::Operation)
        })
        .collect()
}

pub(super) fn atomic_restore_checkout_path<FileSystem: WorkspaceFileSystem>(
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    authority: &PinnedRepository,
    original_prefix: &Path,
    path: &Path,
    expected: &BTreeMap<PathBuf, WorktreeRollbackEntry>,
    expected_identities: Option<&WorktreeRollbackIdentities>,
) -> Result<(), LocalGitFailure> {
    let original_path = original_prefix.join(path);
    let (workspace_parent, workspace_leaf) = open_worktree_parent(&authority.root, path)?;
    let (original_parent, original_leaf) = open_worktree_parent(&authority.root, &original_path)?;
    let original_exists = descriptor_entry_exists(&original_parent, &original_leaf)?;
    let workspace_exists = descriptor_entry_exists(&workspace_parent, &workspace_leaf)?;
    match (original_exists, workspace_exists) {
        (true, true) => {
            renameat_with(
                &original_parent,
                &original_leaf,
                &workspace_parent,
                &workspace_leaf,
                RenameFlags::EXCHANGE,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            let observed = capture_rollback_subtree(filesystem, root, original_prefix, path);
            let observed_identities = observed.as_ref().ok().and_then(|observed| {
                expected_identities.map(|_| {
                    capture_rollback_identities(&authority.root, original_prefix, observed)
                })
            });
            if observed.as_ref().map_or(true, |observed| {
                !rollback_states_equal(observed, expected)
                    || !rollback_identities_equal(observed_identities.as_ref(), expected_identities)
            }) {
                renameat_with(
                    &original_parent,
                    &original_leaf,
                    &workspace_parent,
                    &workspace_leaf,
                    RenameFlags::EXCHANGE,
                )
                .map_err(|_| LocalGitFailure::Operation)?;
                return Ok(());
            }
        }
        (true, false) => {
            if expected.get(path) != Some(&WorktreeRollbackEntry::Missing) {
                return Ok(());
            }
            renameat_with(
                &original_parent,
                &original_leaf,
                &workspace_parent,
                &workspace_leaf,
                RenameFlags::NOREPLACE,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
        }
        (false, true) => {
            let descriptor = openat(
                &original_parent,
                &original_leaf,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            let sentinel = fs::File::from(descriptor);
            let sentinel_identity = file_identity(
                &sentinel
                    .metadata()
                    .map_err(|_| LocalGitFailure::Operation)?,
            );
            renameat_with(
                &original_parent,
                &original_leaf,
                &workspace_parent,
                &workspace_leaf,
                RenameFlags::EXCHANGE,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            let observed = capture_rollback_subtree(filesystem, root, original_prefix, path);
            let observed_identities = observed.as_ref().ok().and_then(|observed| {
                expected_identities.map(|_| {
                    capture_rollback_identities(&authority.root, original_prefix, observed)
                })
            });
            if observed.as_ref().map_or(true, |observed| {
                !rollback_states_equal(observed, expected)
                    || !rollback_identities_equal(observed_identities.as_ref(), expected_identities)
            }) {
                renameat_with(
                    &original_parent,
                    &original_leaf,
                    &workspace_parent,
                    &workspace_leaf,
                    RenameFlags::EXCHANGE,
                )
                .map_err(|_| LocalGitFailure::Operation)?;
                return Ok(());
            }
            let current_identity = statat(
                &workspace_parent,
                &workspace_leaf,
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map(|metadata| FileIdentity {
                device: metadata.st_dev,
                inode: metadata.st_ino,
            })
            .map_err(|_| LocalGitFailure::Operation)?;
            if current_identity != sentinel_identity {
                return Err(LocalGitFailure::Operation);
            }
            unlinkat(&workspace_parent, &workspace_leaf, AtFlags::empty())
                .map_err(|_| LocalGitFailure::Operation)?;
        }
        (false, false) => {
            if expected.get(path) != Some(&WorktreeRollbackEntry::Missing) {
                return Ok(());
            }
        }
    }
    Ok(())
}

pub(super) fn capture_rollback_identities(
    root: &fs::File,
    prefix: &Path,
    state: &BTreeMap<PathBuf, WorktreeRollbackEntry>,
) -> Result<WorktreeRollbackIdentities, LocalGitFailure> {
    state
        .iter()
        .map(|(path, entry)| {
            let identity = if entry == &WorktreeRollbackEntry::Missing {
                None
            } else {
                let full_path = prefix.join(path);
                let (parent, leaf) = open_worktree_parent(root, &full_path)?;
                Some(
                    statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW)
                        .map(|metadata| FileIdentity {
                            device: metadata.st_dev,
                            inode: metadata.st_ino,
                        })
                        .map_err(|_| LocalGitFailure::Operation)?,
                )
            };
            Ok((path.clone(), identity))
        })
        .collect()
}

fn rollback_identities_equal(
    observed: Option<&Result<WorktreeRollbackIdentities, LocalGitFailure>>,
    expected: Option<&WorktreeRollbackIdentities>,
) -> bool {
    match (observed, expected) {
        (Some(Ok(observed)), Some(expected)) => observed == expected,
        (None, None) => true,
        (Some(Err(_)) | None, Some(_)) | (Some(_), None) => false,
    }
}

pub(super) fn open_worktree_parent(
    root: &fs::File,
    path: &Path,
) -> Result<(OwnedFd, OsString), LocalGitFailure> {
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .ok_or(LocalGitFailure::Operation)?
        .to_owned();
    let mut directory = dup(root).map_err(|_| LocalGitFailure::Operation)?;
    for component in path.parent().unwrap_or_else(|| Path::new("")).components() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        directory = openat(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
    }
    Ok((directory, leaf))
}

pub(super) fn rollback_states_equal(
    observed: &BTreeMap<PathBuf, WorktreeRollbackEntry>,
    expected: &BTreeMap<PathBuf, WorktreeRollbackEntry>,
) -> bool {
    observed.len() == expected.len()
        && observed.iter().all(|(path, observed)| {
            expected
                .get(path)
                .is_some_and(|expected| match (observed, expected) {
                    (
                        WorktreeRollbackEntry::File {
                            bytes: observed_bytes,
                            mode: observed_mode,
                        },
                        WorktreeRollbackEntry::File {
                            bytes: expected_bytes,
                            mode: expected_mode,
                        },
                    ) => {
                        observed_bytes == expected_bytes
                            && observed_mode & 0o111 == expected_mode & 0o111
                    }
                    _ => observed == expected,
                })
        })
}

pub(super) struct CheckoutRollbackContext<'context, FileSystem> {
    pub(super) filesystem: &'context FileSystem,
    pub(super) root: &'context WorkspaceRoot,
    pub(super) authority: &'context PinnedRepository,
}

pub(super) fn checkout_tree_with_rollback<
    FileSystem: WorkspaceFileSystem,
    Checkout: FnOnce() -> Result<(), LocalGitFailure>,
>(
    repository: &Repository,
    current_tree: Option<&git2::Tree<'_>>,
    target_tree: &git2::Tree<'_>,
    updated_paths: &RefCell<BTreeSet<PathBuf>>,
    rollback: CheckoutRollbackContext<'_, FileSystem>,
    checkout: Checkout,
) -> Result<(), LocalGitFailure> {
    if checkout().is_err() {
        let rollback_paths = updated_paths.borrow().clone();
        if !rollback_paths.is_empty() {
            rollback_checkout_atomically(
                repository,
                current_tree,
                target_tree,
                &rollback_paths,
                rollback,
                None,
            )?;
        }
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

pub(super) fn restore_index(
    authority: &PinnedRepository,
    original_bytes: &[u8],
    expected_identity: FileIdentity,
) -> Result<(), LocalGitFailure> {
    let (mut lock, _replacement) = IndexLock::acquire_for_repository(authority)?;
    let current_identity = fs::symlink_metadata(authority.git_path("index"))
        .map(|metadata| file_identity(&metadata))
        .map_err(|_| LocalGitFailure::Operation)?;
    if current_identity != expected_identity {
        return Err(LocalGitFailure::Operation);
    }
    lock.write_raw(original_bytes)?;
    lock.commit().map(|_| ())
}

pub(super) fn validate_checkout_path<FileSystem: WorkspaceFileSystem>(
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    path: &Path,
    current_index: &Index,
    target_tree: &git2::Tree<'_>,
) -> Result<(), LocalGitFailure> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    match filesystem.entry_kind(root, parent) {
        Ok(WorkspaceEntryKind::Directory) => {}
        Ok(_)
            if current_index.get_path(parent, 0).is_some()
                && target_tree
                    .get_path(parent)
                    .is_ok_and(|entry| entry.kind() == Some(git2::ObjectType::Tree)) => {}
        Ok(_) | Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
        Err(WorkspaceResolveError::Io { .. }) => {}
    }
    match filesystem.entry_kind(root, path) {
        Ok(WorkspaceEntryKind::Symlink | WorkspaceEntryKind::Other) => Err(LocalGitFailure::Path),
        Ok(WorkspaceEntryKind::File | WorkspaceEntryKind::Directory)
        | Err(WorkspaceResolveError::Io { .. }) => Ok(()),
        Err(WorkspaceResolveError::Rejected(_)) => Err(LocalGitFailure::Path),
    }
}
