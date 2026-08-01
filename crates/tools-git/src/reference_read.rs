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

use crate::failure::LocalGitFailure;
use crate::limits::MAX_REVISION_BYTES;
use crate::packed_reference::packed_reference_target;
use crate::pinning::PinnedRepository;
use crate::reference_lock::{PinnedReferenceValue, ReferenceLock, open_reference_parent};

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
    let bound = match open_reference_parent(authority, name, false) {
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
            return Ok(PinnedReferenceValue::Missing);
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
        let symbolic = std::str::from_utf8(symbolic).map_err(|_| LocalGitFailure::Operation)?;
        if !symbolic.starts_with("refs/") || !git2::Reference::is_valid_name(symbolic) {
            return Err(LocalGitFailure::Operation);
        }
        return Ok(PinnedReferenceValue::Symbolic(symbolic.to_owned()));
    }
    let direct = std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| git2::Oid::from_str(value).ok())
        .ok_or(LocalGitFailure::Operation)?;
    Ok(PinnedReferenceValue::Direct(direct))
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
