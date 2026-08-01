use std::{
    ffi::OsString,
    fs,
    io::Read,
    os::{fd::OwnedFd, unix::ffi::OsStringExt},
    path::{Component, Path, PathBuf},
};

use git2::Repository;
use rustix::{
    fs::{Mode, OFlags, openat},
    io::dup,
};

use crate::bounded::{bounded_bytes, tree_for_commit};
use crate::failure::LocalGitFailure;
use crate::limits::{MAX_BRANCH_BYTES, MAX_REVISION_BYTES};
use crate::packed_reference::packed_reference_target;
use crate::pinning::PinnedRepository;
#[cfg(test)]
use git2::ErrorCode;

pub(super) fn status_head(
    authority: &PinnedRepository,
) -> Result<(Option<String>, bool, Option<git2::Oid>), LocalGitFailure> {
    let value = read_status_reference(authority, b"HEAD")?;
    let StatusReferenceValue::Symbolic(target) = value else {
        return match value {
            StatusReferenceValue::Direct(target) => Ok((None, false, Some(target))),
            StatusReferenceValue::Missing | StatusReferenceValue::Symbolic(_) => {
                Err(LocalGitFailure::Operation)
            }
        };
    };
    let branch = target.strip_prefix(b"refs/heads/");
    let (branch, branch_truncated) = match branch {
        Some(branch) => {
            let (branch, truncated) = bounded_bytes(branch, MAX_BRANCH_BYTES);
            (Some(branch), truncated)
        }
        None => (None, false),
    };
    let target = resolve_status_reference_chain(authority, target)?;
    Ok((branch, branch_truncated, target))
}

#[cfg(test)]
pub(super) fn status_head_from_reference(
    head: &git2::Reference<'_>,
) -> Result<(Option<String>, bool, Option<git2::Oid>), LocalGitFailure> {
    let branch = head
        .symbolic_target_bytes()
        .and_then(|target| target.strip_prefix(b"refs/heads/"));
    let (branch, branch_truncated) = match branch {
        Some(branch) => {
            let (branch, truncated) = bounded_bytes(branch, MAX_BRANCH_BYTES);
            (Some(branch), truncated)
        }
        None => (None, false),
    };
    let target = match head.target() {
        Some(target) => Some(target),
        None => match head.resolve() {
            Ok(resolved) => Some(resolved.target().ok_or(LocalGitFailure::Operation)?),
            Err(error) if error.code() == ErrorCode::NotFound => None,
            Err(_) => return Err(LocalGitFailure::Operation),
        },
    };
    Ok((branch, branch_truncated, target))
}

pub(super) enum StatusReferenceValue {
    Direct(git2::Oid),
    Symbolic(Vec<u8>),
    Missing,
}

pub(super) fn resolve_status_reference_chain(
    authority: &PinnedRepository,
    start: Vec<u8>,
) -> Result<Option<git2::Oid>, LocalGitFailure> {
    const MAX_SYMBOLIC_REFERENCE_DEPTH: usize = 16;
    let mut names = Vec::new();
    let mut current = start;
    loop {
        if names.len() == MAX_SYMBOLIC_REFERENCE_DEPTH || names.contains(&current) {
            return Err(LocalGitFailure::Operation);
        }
        let value = read_status_reference(authority, &current)?;
        names.push(current);
        match value {
            StatusReferenceValue::Direct(oid) => return Ok(Some(oid)),
            StatusReferenceValue::Symbolic(target) => current = target,
            StatusReferenceValue::Missing => return Ok(None),
        }
    }
}

pub(super) fn read_status_reference(
    authority: &PinnedRepository,
    name: &[u8],
) -> Result<StatusReferenceValue, LocalGitFailure> {
    let Some((parent, leaf)) = open_status_reference_parent(authority, name)? else {
        return status_packed_reference(authority, name);
    };
    let descriptor = match openat(
        &parent,
        &leaf,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => {
            return status_packed_reference(authority, name);
        }
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    let mut file = fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if !metadata.is_file() || metadata.len() > MAX_REVISION_BYTES as u64 {
        return Err(LocalGitFailure::Operation);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_REVISION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    if let Some(symbolic) = bytes.strip_prefix(b"ref: ") {
        status_reference_path(symbolic)?;
        return Ok(StatusReferenceValue::Symbolic(symbolic.to_vec()));
    }
    let direct = std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| git2::Oid::from_str(value).ok())
        .ok_or(LocalGitFailure::Operation)?;
    Ok(StatusReferenceValue::Direct(direct))
}

pub(super) fn open_status_reference_parent(
    authority: &PinnedRepository,
    name: &[u8],
) -> Result<Option<(OwnedFd, OsString)>, LocalGitFailure> {
    let path = status_reference_path(name)?;
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .ok_or(LocalGitFailure::Operation)?
        .to_owned();
    let mut directory = dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
    for component in path.parent().unwrap_or_else(|| Path::new("")).components() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        directory = match openat(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
            Err(_) => return Err(LocalGitFailure::Operation),
        };
    }
    Ok(Some((directory, leaf)))
}

pub(super) fn status_reference_path(name: &[u8]) -> Result<PathBuf, LocalGitFailure> {
    if name != b"HEAD" && !name.starts_with(b"refs/") {
        return Err(LocalGitFailure::Operation);
    }
    let path = PathBuf::from(OsString::from_vec(name.to_vec()));
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(LocalGitFailure::Operation);
    }
    Ok(path)
}

pub(super) fn status_packed_reference(
    authority: &PinnedRepository,
    name: &[u8],
) -> Result<StatusReferenceValue, LocalGitFailure> {
    let Some(name) = std::str::from_utf8(name).ok() else {
        return Ok(StatusReferenceValue::Missing);
    };
    packed_reference_target(authority, name)
        .map(|target| target.map_or(StatusReferenceValue::Missing, StatusReferenceValue::Direct))
}

pub(super) fn worktree_head_tree<'repository>(
    repository: &'repository Repository,
    authority: &PinnedRepository,
) -> Result<Option<git2::Tree<'repository>>, LocalGitFailure> {
    let (_, _, target) = status_head(authority)?;
    target
        .map(|target| tree_for_commit(repository, target))
        .transpose()
}
