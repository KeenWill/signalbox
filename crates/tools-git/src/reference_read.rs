use std::{
    ffi::OsStr,
    fs,
    io::Read,
    os::fd::OwnedFd,
    path::{Component, Path},
};

use rustix::{
    fs::{Mode, OFlags, openat},
    io::dup,
};

use crate::descriptor::file_snapshot_identity;
use crate::failure::LocalGitFailure;
use crate::limits::MAX_REVISION_BYTES;
use crate::packed_reference::packed_reference_target;
use crate::pinning::PinnedRepository;
use crate::reference_lock::{
    PinnedReferenceValue, ReferenceLock, ReferenceParentMode, open_reference_parent,
};

pub(super) fn open_git_directory_path(
    authority: &PinnedRepository,
    relative: &Path,
) -> Result<OwnedFd, LocalGitFailure> {
    let mut directory = dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
    for component in relative.components() {
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
    Ok(directory)
}

pub(super) fn read_pinned_reference(
    authority: &PinnedRepository,
    name: &str,
) -> Result<PinnedReferenceValue, LocalGitFailure> {
    let bound = match open_reference_parent(authority, name, ReferenceParentMode::ExistingOnly) {
        Ok(bound) => bound,
        Err(error) if name.starts_with("refs/") => {
            if loose_reference_parent_is_missing(authority, name)? {
                return packed_reference_target(authority, name).map(|target| {
                    target.map_or(PinnedReferenceValue::Missing, PinnedReferenceValue::Direct)
                });
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    read_reference_leaf(&bound.directory, &bound.leaf, authority, name)
}

pub(super) fn loose_reference_parent_is_missing(
    authority: &PinnedRepository,
    name: &str,
) -> Result<bool, LocalGitFailure> {
    let mut directory = dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
    for component in Path::new(name)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .components()
    {
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
            Err(error) if error == rustix::io::Errno::NOENT => return Ok(true),
            Err(_) => return Err(LocalGitFailure::Operation),
        };
    }
    Ok(false)
}

pub(super) fn read_reference_leaf(
    parent: &OwnedFd,
    leaf: &OsStr,
    authority: &PinnedRepository,
    name: &str,
) -> Result<PinnedReferenceValue, LocalGitFailure> {
    read_reference_leaf_with_hook(parent, leaf, authority, name, || {})
}

fn read_reference_leaf_with_hook<Hook: FnOnce()>(
    parent: &OwnedFd,
    leaf: &OsStr,
    authority: &PinnedRepository,
    name: &str,
    after_metadata: Hook,
) -> Result<PinnedReferenceValue, LocalGitFailure> {
    authority.validate_supported_layout()?;
    let descriptor = match openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT && name.starts_with("refs/") => {
            return packed_reference_target(authority, name).map(|target| {
                target.map_or(PinnedReferenceValue::Missing, PinnedReferenceValue::Direct)
            });
        }
        Err(error) if error == rustix::io::Errno::NOENT => {
            authority.validate_supported_layout()?;
            return Ok(PinnedReferenceValue::Missing);
        }
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    let mut file = fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if !metadata.is_file() || metadata.len() > MAX_REVISION_BYTES as u64 {
        return Err(LocalGitFailure::Operation);
    }
    after_metadata();
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_REVISION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    let after_read = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if bytes.len() > MAX_REVISION_BYTES
        || bytes.len() as u64 != metadata.len()
        || file_snapshot_identity(&metadata) != file_snapshot_identity(&after_read)
    {
        return Err(LocalGitFailure::Operation);
    }
    authority.validate_supported_layout()?;
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    if let Some(symbolic) = bytes.strip_prefix(b"ref: ") {
        let symbolic = std::str::from_utf8(symbolic).map_err(|_| LocalGitFailure::Operation)?;
        if !symbolic.starts_with("refs/") || !git2::Reference::is_valid_name(symbolic) {
            return Err(LocalGitFailure::Operation);
        }
        return Ok(PinnedReferenceValue::Symbolic(symbolic.to_owned()));
    }
    let direct = std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| git2::Oid::from_str_ext(value, authority.object_format).ok())
        .ok_or(LocalGitFailure::Operation)?;
    Ok(PinnedReferenceValue::Direct(direct))
}

#[cfg(test)]
pub(super) fn read_reference_leaf_with_test_hook<Hook: FnOnce()>(
    parent: &OwnedFd,
    leaf: &OsStr,
    authority: &PinnedRepository,
    name: &str,
    after_metadata: Hook,
) -> Result<PinnedReferenceValue, LocalGitFailure> {
    read_reference_leaf_with_hook(parent, leaf, authority, name, after_metadata)
}

pub(super) fn resolve_pinned_reference_chain(
    authority: &PinnedRepository,
    locks: Option<&[ReferenceLock]>,
) -> Result<(Vec<String>, Option<git2::Oid>), LocalGitFailure> {
    resolve_pinned_reference_chain_from(authority, "HEAD", locks)
}

pub(super) fn resolve_pinned_reference_chain_from(
    authority: &PinnedRepository,
    start: &str,
    locks: Option<&[ReferenceLock]>,
) -> Result<(Vec<String>, Option<git2::Oid>), LocalGitFailure> {
    const MAX_SYMBOLIC_REFERENCE_DEPTH: usize = 16;
    let mut names = Vec::new();
    let mut current = start.to_owned();
    loop {
        if names.len() == MAX_SYMBOLIC_REFERENCE_DEPTH || names.contains(&current) {
            return Err(LocalGitFailure::Operation);
        }
        let value = match locks {
            Some(locks) => locks
                .iter()
                .find(|lock| lock.name == current)
                .ok_or(LocalGitFailure::Operation)?
                .read(authority)?,
            None => read_pinned_reference(authority, &current)?,
        };
        names.push(current);
        match value {
            PinnedReferenceValue::Direct(oid) => return Ok((names, Some(oid))),
            PinnedReferenceValue::Symbolic(target) => current = target,
            PinnedReferenceValue::Missing => return Ok((names, None)),
        }
    }
}
