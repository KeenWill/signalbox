use std::{collections::BTreeMap, os::unix::ffi::OsStrExt, path::PathBuf};

use git2::{Index, Repository};

use crate::failure::LocalGitFailure;
use crate::limits::{
    GITLINK_MODE, MAX_INDEX_ENTRIES, MAX_OBJECT_BYTES, MAX_TREE_BLOB_BYTES,
    MAX_WORKTREE_INSPECTIONS, MAX_WORKTREE_PATH_BYTES,
};
use crate::pinning::PinnedRepository;
use crate::reference_read::resolve_pinned_reference_chain_from;

pub(super) fn tree_files(
    repository: &Repository,
    root: &git2::Tree<'_>,
) -> Result<BTreeMap<PathBuf, (git2::Oid, u32)>, LocalGitFailure> {
    let mut pending = vec![(root.id(), PathBuf::new())];
    let mut files = BTreeMap::new();
    while let Some((oid, prefix)) = pending.pop() {
        let tree = repository
            .find_tree(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        for entry in &tree {
            let mut path = prefix.clone();
            path.push(std::ffi::OsStr::from_bytes(entry.name_bytes()));
            match entry.kind() {
                Some(git2::ObjectType::Tree) => pending.push((entry.id(), path)),
                Some(git2::ObjectType::Blob) => {
                    let mode =
                        u32::try_from(entry.filemode()).map_err(|_| LocalGitFailure::Operation)?;
                    files.insert(path, (entry.id(), mode));
                }
                Some(git2::ObjectType::Commit) if entry.filemode() == GITLINK_MODE as i32 => {
                    files.insert(path, (entry.id(), GITLINK_MODE));
                }
                _ => return Err(LocalGitFailure::Operation),
            }
        }
    }
    Ok(files)
}

pub(super) fn validate_tree_discovery(
    repository: &Repository,
    root: &git2::Tree<'_>,
) -> Result<(), LocalGitFailure> {
    validate_tree_discovery_with_symlinks(repository, root, true)
}

pub(super) fn validate_checkout_tree_discovery(
    repository: &Repository,
    root: &git2::Tree<'_>,
) -> Result<(), LocalGitFailure> {
    validate_tree_discovery_with_symlinks(repository, root, false)
}

pub(super) fn validate_tree_discovery_with_symlinks(
    repository: &Repository,
    root: &git2::Tree<'_>,
    allow_symlinks: bool,
) -> Result<(), LocalGitFailure> {
    let object_database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
    let mut pending = vec![(root.id(), PathBuf::new())];
    let mut inspected = 0_usize;
    let mut inspected_path_bytes = 0_usize;
    let mut inspected_blob_bytes = 0_usize;
    while let Some((oid, prefix)) = pending.pop() {
        let (size, kind) = object_database
            .read_header(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        if kind != git2::ObjectType::Tree || size > MAX_OBJECT_BYTES {
            return Err(LocalGitFailure::Operation);
        }
        let tree = repository
            .find_tree(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        for entry in &tree {
            inspected = inspected.saturating_add(1);
            let mut path = prefix.clone();
            path.push(std::ffi::OsStr::from_bytes(entry.name_bytes()));
            inspected_path_bytes =
                inspected_path_bytes.saturating_add(path.as_os_str().as_bytes().len());
            if inspected > MAX_WORKTREE_INSPECTIONS
                || inspected_path_bytes > MAX_WORKTREE_PATH_BYTES
            {
                return Err(LocalGitFailure::Operation);
            }
            match entry.kind() {
                Some(git2::ObjectType::Tree) => pending.push((entry.id(), path)),
                Some(git2::ObjectType::Blob) => {
                    if !matches!(entry.filemode(), 0o100644 | 0o100755)
                        && !(allow_symlinks && entry.filemode() == 0o120000)
                    {
                        return Err(LocalGitFailure::Operation);
                    }
                    let (size, kind) = object_database
                        .read_header(entry.id())
                        .map_err(|_| LocalGitFailure::Operation)?;
                    inspected_blob_bytes = inspected_blob_bytes.saturating_add(size);
                    if kind != git2::ObjectType::Blob
                        || size > MAX_OBJECT_BYTES
                        || inspected_blob_bytes > MAX_TREE_BLOB_BYTES
                    {
                        return Err(LocalGitFailure::Operation);
                    }
                }
                Some(git2::ObjectType::Commit) if entry.filemode() == GITLINK_MODE as i32 => {}
                _ => return Err(LocalGitFailure::Operation),
            }
        }
    }
    Ok(())
}

pub(super) fn validate_index_entry_count(index: &Index) -> Result<(), LocalGitFailure> {
    if index.len() > MAX_INDEX_ENTRIES {
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

pub(super) fn validate_index_objects(
    repository: &Repository,
    index: &Index,
) -> Result<(), LocalGitFailure> {
    validate_index_entry_count(index)?;
    let object_database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
    let mut blob_bytes = 0_usize;
    for entry in index.iter().filter(|entry| entry.flags & 0x3000 == 0) {
        if entry.mode == GITLINK_MODE {
            continue;
        }
        let (size, kind) = object_database
            .read_header(entry.id)
            .map_err(|_| LocalGitFailure::Operation)?;
        blob_bytes = blob_bytes.saturating_add(size);
        if kind != git2::ObjectType::Blob
            || size > MAX_OBJECT_BYTES
            || blob_bytes > MAX_TREE_BLOB_BYTES
        {
            return Err(LocalGitFailure::Operation);
        }
    }
    Ok(())
}

pub(super) fn validate_object_header(
    repository: &Repository,
    oid: git2::Oid,
) -> Result<git2::ObjectType, LocalGitFailure> {
    let (size, kind) = repository
        .odb()
        .and_then(|object_database| object_database.read_header(oid))
        .map_err(|_| LocalGitFailure::Operation)?;
    if size > MAX_OBJECT_BYTES {
        Err(LocalGitFailure::Operation)
    } else {
        Ok(kind)
    }
}

pub(super) fn find_bounded_commit(
    repository: &Repository,
    oid: git2::Oid,
) -> Result<git2::Commit<'_>, LocalGitFailure> {
    if validate_object_header(repository, oid)? != git2::ObjectType::Commit {
        return Err(LocalGitFailure::Operation);
    }
    repository
        .find_commit(oid)
        .map_err(|_| LocalGitFailure::Operation)
}

pub(super) fn find_bounded_tree(
    repository: &Repository,
    oid: git2::Oid,
) -> Result<git2::Tree<'_>, LocalGitFailure> {
    if validate_object_header(repository, oid)? != git2::ObjectType::Tree {
        return Err(LocalGitFailure::Operation);
    }
    repository
        .find_tree(oid)
        .map_err(|_| LocalGitFailure::Operation)
}

pub(super) fn tree_for_commit(
    repository: &Repository,
    oid: git2::Oid,
) -> Result<git2::Tree<'_>, LocalGitFailure> {
    let commit = find_bounded_commit(repository, oid)?;
    find_bounded_tree(repository, commit.tree_id())
}

pub(super) fn resolve_bounded_commit<'repository>(
    repository: &'repository Repository,
    authority: &PinnedRepository,
    revision: &str,
) -> Result<git2::Commit<'repository>, LocalGitFailure> {
    let mut oid = resolve_exact_revision_oid(authority, revision)?;
    for _depth in 0..16 {
        match validate_object_header(repository, oid)? {
            git2::ObjectType::Commit => return find_bounded_commit(repository, oid),
            git2::ObjectType::Tag => {
                oid = repository
                    .find_tag(oid)
                    .map_err(|_| LocalGitFailure::Operation)?
                    .target_id();
            }
            _ => return Err(LocalGitFailure::Operation),
        }
    }
    Err(LocalGitFailure::Operation)
}

pub(super) fn resolve_bounded_tree<'repository>(
    repository: &'repository Repository,
    authority: &PinnedRepository,
    revision: &str,
) -> Result<git2::Tree<'repository>, LocalGitFailure> {
    let mut oid = resolve_exact_revision_oid(authority, revision)?;
    for _depth in 0..16 {
        match validate_object_header(repository, oid)? {
            git2::ObjectType::Commit => return tree_for_commit(repository, oid),
            git2::ObjectType::Tree => return find_bounded_tree(repository, oid),
            git2::ObjectType::Tag => {
                oid = repository
                    .find_tag(oid)
                    .map_err(|_| LocalGitFailure::Operation)?
                    .target_id();
            }
            _ => return Err(LocalGitFailure::Operation),
        }
    }
    Err(LocalGitFailure::Operation)
}

pub(super) fn resolve_exact_revision_oid(
    authority: &PinnedRepository,
    revision: &str,
) -> Result<git2::Oid, LocalGitFailure> {
    if let Some(oid) = crate::layout::parse_full_object_id(revision, authority.object_format) {
        return Ok(oid);
    }
    let (_, target) = resolve_pinned_reference_chain_from(authority, revision, None)?;
    target.ok_or(LocalGitFailure::Operation)
}

pub(super) fn bounded_text(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (value[..boundary].to_owned(), true)
}

pub(super) fn bounded_bytes(value: &[u8], max_bytes: usize) -> (String, bool) {
    match std::str::from_utf8(value) {
        Ok(value) => bounded_text(value, max_bytes),
        Err(_) => {
            let lossy = String::from_utf8_lossy(value);
            let (value, _) = bounded_text(&lossy, max_bytes);
            (value, true)
        }
    }
}
