use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use git2::{Delta, DiffFindOptions, Index, ObjectFormat, ObjectType, Repository};
use signalbox_tools_workspace::{
    WorkspaceEntryKind, WorkspaceFileSystem, WorkspacePathRejection, WorkspaceResolveError,
    WorkspaceRoot,
};

use crate::bounded::{
    bounded_text, tree_for_commit, validate_index_objects, validate_tree_discovery,
};
use crate::diff::read_worktree_symlink;
use crate::failure::LocalGitFailure;
use crate::limits::{
    GITLINK_MODE, INDEX_ASSUME_VALID, INDEX_SKIP_WORKTREE, MAX_OBJECT_BYTES, MAX_STATUS_ENTRIES,
    MAX_STATUS_PATH_BYTES, MAX_WORKTREE_TOTAL_BYTES,
};
use crate::pinning::{PinnedRepository, repository_filemode};
use crate::result::{StatusEntry, StatusResult};
use crate::status_reference::StatusHeadSnapshot;

pub(super) fn status<FileSystem: WorkspaceFileSystem>(
    repository: &Repository,
    authority: &PinnedRepository,
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    untracked: Vec<PathBuf>,
) -> Result<StatusResult, LocalGitFailure> {
    let head_snapshot = StatusHeadSnapshot::capture(authority)?;
    let branch = head_snapshot.branch.clone();
    let branch_truncated = head_snapshot.branch_truncated;
    let head_oid = head_snapshot.target;
    let head = head_oid.map(|oid| oid.to_string());
    let head_tree = head_oid
        .map(|oid| tree_for_commit(repository, oid))
        .transpose()?;
    let index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
    if let Some(head_tree) = &head_tree {
        validate_tree_discovery(repository, head_tree)?;
    }
    validate_index_objects(repository, &index)?;
    let mut staged = repository
        .diff_tree_to_index(head_tree.as_ref(), Some(&index), None)
        .map_err(|_| LocalGitFailure::Operation)?;
    staged
        .find_similar(Some(DiffFindOptions::new().renames(true)))
        .map_err(|_| LocalGitFailure::Operation)?;
    let filemode = repository_filemode(repository)?;
    let mut worktree_bytes = 0_usize;
    let mut raw = BTreeMap::new();
    for delta in staged.deltas() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .ok_or(LocalGitFailure::Operation)?
            .to_owned();
        let previous_path = (delta.status() == Delta::Renamed)
            .then(|| delta.old_file().path().map(Path::to_owned))
            .flatten();
        raw.insert(
            path,
            RawStatusEntry {
                previous_path,
                index: delta_status(delta.status()),
                worktree: "unchanged",
            },
        );
    }
    let indexed = index_files(&index);
    let mut deleted = Vec::new();
    for (path, (oid, mode)) in &indexed {
        if *mode == GITLINK_MODE {
            match filesystem.entry_kind(root, path) {
                Ok(WorkspaceEntryKind::Directory) => {
                    return Err(LocalGitFailure::Operation);
                }
                Err(WorkspaceResolveError::Io { source, .. })
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    set_worktree_status(&mut raw, path, "deleted");
                }
                Ok(WorkspaceEntryKind::File) => {
                    set_worktree_status(&mut raw, path, "type_changed");
                }
                Ok(WorkspaceEntryKind::Symlink | WorkspaceEntryKind::Other)
                | Err(WorkspaceResolveError::Rejected(_)) => {
                    return Err(LocalGitFailure::Path);
                }
                Err(WorkspaceResolveError::Io { .. }) => {
                    return Err(LocalGitFailure::Operation);
                }
            }
            continue;
        }
        if matches!(
            filesystem.entry_kind(root, path),
            Ok(WorkspaceEntryKind::Symlink)
                | Err(WorkspaceResolveError::Rejected(
                    WorkspacePathRejection::Symlink
                ))
        ) {
            let bytes = read_worktree_symlink(authority, path, MAX_OBJECT_BYTES)?;
            charge_worktree_bytes(&mut worktree_bytes, bytes.len())?;
            if *mode != 0o120000 {
                set_worktree_status(&mut raw, path, "type_changed");
            } else if blob_oid(&bytes, authority.object_format)? != *oid {
                set_worktree_status(&mut raw, path, "modified");
            }
            continue;
        }
        match filesystem.read_file_prefix(root, path, MAX_OBJECT_BYTES) {
            Ok(read) => {
                charge_worktree_bytes(&mut worktree_bytes, read.bytes.len())?;
                let observed_mode = if read.mode & 0o111 == 0 {
                    0o100644
                } else {
                    0o100755
                };
                if *mode == 0o120000 {
                    set_worktree_status(&mut raw, path, "type_changed");
                } else if read.truncated
                    || blob_oid(&read.bytes, authority.object_format)? != *oid
                    || (filemode && observed_mode != *mode)
                {
                    set_worktree_status(&mut raw, path, "modified");
                }
            }
            Err(WorkspaceResolveError::Io { source, .. })
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                deleted.push((path.clone(), *oid));
                set_worktree_status(&mut raw, path, "deleted");
            }
            Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
            Err(WorkspaceResolveError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::IsADirectory =>
            {
                set_worktree_status(&mut raw, path, "type_changed");
            }
            Err(WorkspaceResolveError::Io { .. }) => return Err(LocalGitFailure::Operation),
        }
    }
    for path in untracked {
        let rename = match filesystem.entry_kind(root, &path) {
            Ok(WorkspaceEntryKind::File) => {
                match filesystem.read_file_prefix(root, &path, MAX_OBJECT_BYTES) {
                    Ok(read) => {
                        charge_worktree_bytes(&mut worktree_bytes, read.bytes.len())?;
                        (!read.truncated)
                            .then(|| blob_oid(&read.bytes, authority.object_format).ok())
                            .flatten()
                            .and_then(|oid| {
                                deleted
                                    .iter()
                                    .position(|(_, deleted_oid)| *deleted_oid == oid)
                            })
                    }
                    Err(WorkspaceResolveError::Rejected(_)) => {
                        return Err(LocalGitFailure::Path);
                    }
                    Err(WorkspaceResolveError::Io { .. }) => None,
                }
            }
            Ok(WorkspaceEntryKind::Directory)
            | Ok(WorkspaceEntryKind::Symlink)
            | Ok(WorkspaceEntryKind::Other)
            | Err(_) => None,
        };
        if let Some(position) = rename {
            let (previous_path, _) = deleted.remove(position);
            let staged_delta = raw
                .get(&previous_path)
                .is_some_and(|entry| entry.index != "unchanged");
            if !staged_delta {
                raw.remove(&previous_path);
            }
            raw.insert(
                path,
                RawStatusEntry {
                    previous_path: Some(previous_path),
                    index: "unchanged",
                    worktree: "renamed",
                },
            );
        } else {
            raw.entry(path)
                .and_modify(|entry| entry.worktree = "untracked")
                .or_insert(RawStatusEntry {
                    previous_path: None,
                    index: "unchanged",
                    worktree: "untracked",
                });
        }
    }
    let mut truncated = raw.len() > MAX_STATUS_ENTRIES;
    let mut entries = Vec::new();
    for (path, entry) in raw.into_iter().take(MAX_STATUS_ENTRIES) {
        let (path, path_truncated) = bounded_status_path(path.as_os_str().as_bytes());
        let (previous_path, previous_truncated) =
            entry.previous_path.map_or((None, false), |path| {
                let (path, truncated) = bounded_status_path(path.as_os_str().as_bytes());
                (Some(path), truncated)
            });
        truncated |= path_truncated || previous_truncated;
        entries.push(StatusEntry {
            path,
            previous_path,
            index: entry.index,
            worktree: entry.worktree,
        });
    }
    let result = StatusResult {
        branch,
        branch_truncated,
        head,
        entries,
        truncated,
    };
    head_snapshot.validate(authority)?;
    Ok(result)
}

pub(super) struct RawStatusEntry {
    previous_path: Option<PathBuf>,
    index: &'static str,
    worktree: &'static str,
}

pub(super) fn set_worktree_status(
    entries: &mut BTreeMap<PathBuf, RawStatusEntry>,
    path: &Path,
    worktree: &'static str,
) {
    entries
        .entry(path.to_owned())
        .or_insert(RawStatusEntry {
            previous_path: None,
            index: "unchanged",
            worktree,
        })
        .worktree = worktree;
}

pub(super) const fn delta_status(status: Delta) -> &'static str {
    match status {
        Delta::Added => "added",
        Delta::Deleted => "deleted",
        Delta::Modified => "modified",
        Delta::Renamed => "renamed",
        Delta::Typechange => "type_changed",
        Delta::Conflicted => "conflicted",
        Delta::Unmodified
        | Delta::Copied
        | Delta::Ignored
        | Delta::Untracked
        | Delta::Unreadable => "unchanged",
    }
}

pub(super) fn index_files(index: &Index) -> BTreeMap<PathBuf, (git2::Oid, u32)> {
    index
        .iter()
        .filter(|entry| {
            entry.flags & 0x3000 == 0
                && entry.flags & INDEX_ASSUME_VALID == 0
                && entry.flags_extended & INDEX_SKIP_WORKTREE == 0
        })
        .map(|entry| {
            (
                PathBuf::from(std::ffi::OsString::from_vec(entry.path)),
                (entry.id, entry.mode),
            )
        })
        .collect()
}

pub(super) fn index_backed_worktree_files(index: &Index) -> BTreeMap<PathBuf, (git2::Oid, u32)> {
    index
        .iter()
        .filter(|entry| {
            entry.flags & 0x3000 == 0
                && (entry.flags & INDEX_ASSUME_VALID != 0
                    || entry.flags_extended & INDEX_SKIP_WORKTREE != 0)
        })
        .map(|entry| {
            (
                PathBuf::from(std::ffi::OsString::from_vec(entry.path)),
                (entry.id, entry.mode),
            )
        })
        .collect()
}

pub(super) fn conflicted_index_paths(index: &Index) -> BTreeSet<PathBuf> {
    index
        .iter()
        .filter(|entry| entry.flags & 0x3000 != 0)
        .map(|entry| PathBuf::from(OsString::from_vec(entry.path)))
        .collect()
}

pub(super) fn tracked_directories(index: &Index) -> BTreeSet<PathBuf> {
    let mut directories = BTreeSet::new();
    for path in index
        .iter()
        .map(|entry| PathBuf::from(OsString::from_vec(entry.path)))
    {
        for parent in path.ancestors().skip(1) {
            if !parent.as_os_str().is_empty() {
                directories.insert(parent.to_owned());
            }
        }
    }
    directories
}

pub(super) fn blob_oid(
    bytes: &[u8],
    object_format: ObjectFormat,
) -> Result<git2::Oid, LocalGitFailure> {
    git2::Oid::hash_object_ext(ObjectType::Blob, bytes, object_format)
        .map_err(|_| LocalGitFailure::Operation)
}

pub(super) fn charge_worktree_bytes(
    total: &mut usize,
    bytes: usize,
) -> Result<(), LocalGitFailure> {
    *total = total
        .checked_add(bytes)
        .filter(|total| *total <= MAX_WORKTREE_TOTAL_BYTES)
        .ok_or(LocalGitFailure::Operation)?;
    Ok(())
}

pub(super) fn bounded_status_path(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(path) => bounded_text(path, MAX_STATUS_PATH_BYTES),
        Err(_) => ("[non-utf8]".to_owned(), true),
    }
}
