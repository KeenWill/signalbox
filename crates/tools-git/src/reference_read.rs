use std::{
    ffi::OsStr,
    fs,
    io::Read,
    os::{fd::OwnedFd, unix::fs::FileExt},
    path::{Component, Path},
};

use rustix::{
    fs::{Mode, OFlags, openat},
    io::dup,
};

use crate::descriptor::file_snapshot_identity;
use crate::failure::LocalGitFailure;
use crate::layout::parse_full_object_id;
use crate::limits::MAX_REVISION_BYTES;
use crate::packed_reference::packed_reference_target;
use crate::pinning::PinnedRepository;
use crate::reference_lock::{
    PinnedReferenceValue, ReferenceLock, ReferenceParentMode, open_reference_parent,
    validate_reference_name,
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
    read_pinned_reference_with_hooks(authority, name, || {}, || {}, || {})
}

fn read_pinned_reference_with_hooks<
    AfterMetadata: FnOnce(),
    AfterFirstRead: FnOnce(),
    AfterConfirmation: FnOnce(),
>(
    authority: &PinnedRepository,
    name: &str,
    after_metadata: AfterMetadata,
    after_first_read: AfterFirstRead,
    after_confirmation: AfterConfirmation,
) -> Result<PinnedReferenceValue, LocalGitFailure> {
    validate_reference_name(name)?;
    let bound = match open_reference_parent(authority, name, ReferenceParentMode::ExistingOnly) {
        Ok(bound) => bound,
        Err(error) if name.starts_with("refs/") => {
            if loose_reference_parent_is_missing(authority, name)? {
                let target = packed_reference_target(authority, name)?;
                after_metadata();
                if !loose_reference_parent_is_missing(authority, name)? {
                    return Err(LocalGitFailure::Operation);
                }
                return Ok(
                    target.map_or(PinnedReferenceValue::Missing, PinnedReferenceValue::Direct)
                );
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let value = read_reference_leaf_with_hook(
        &bound.directory,
        &bound.leaf,
        authority,
        name,
        after_metadata,
    )?;
    after_first_read();
    if !bound.hierarchy_is_current(authority) {
        return Err(LocalGitFailure::Operation);
    }
    let confirmed = read_reference_leaf(&bound.directory, &bound.leaf, authority, name)?;
    if confirmed != value {
        return Err(LocalGitFailure::Operation);
    }
    after_confirmation();
    if !bound.hierarchy_is_current(authority) {
        return Err(LocalGitFailure::Operation);
    }
    Ok(value)
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
            let target = packed_reference_target(authority, name)?;
            after_metadata();
            match openat(
                parent,
                leaf,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Err(error) if error == rustix::io::Errno::NOENT => {}
                Ok(_) | Err(_) => return Err(LocalGitFailure::Operation),
            }
            authority.validate_supported_layout()?;
            return Ok(target.map_or(PinnedReferenceValue::Missing, PinnedReferenceValue::Direct));
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
    let mut initial_bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_REVISION_BYTES + 1) as u64)
        .read_to_end(&mut initial_bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    let after_initial_read = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if initial_bytes.len() > MAX_REVISION_BYTES
        || initial_bytes.len() as u64 != metadata.len()
        || file_snapshot_identity(&metadata) != file_snapshot_identity(&after_initial_read)
    {
        return Err(LocalGitFailure::Operation);
    }
    after_metadata();
    let mut bytes = vec![0_u8; initial_bytes.len()];
    file.read_exact_at(&mut bytes, 0)
        .map_err(|_| LocalGitFailure::Operation)?;
    let after_read = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if bytes != initial_bytes
        || file_snapshot_identity(&after_initial_read) != file_snapshot_identity(&after_read)
    {
        return Err(LocalGitFailure::Operation);
    }
    let path_descriptor = openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let path_metadata = fs::File::from(path_descriptor)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    if file_snapshot_identity(&after_read) != file_snapshot_identity(&path_metadata) {
        return Err(LocalGitFailure::Operation);
    }
    authority.validate_supported_layout()?;
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    if let Some(symbolic) = bytes.strip_prefix(b"ref: ") {
        let symbolic = std::str::from_utf8(symbolic).map_err(|_| LocalGitFailure::Operation)?;
        if !symbolic.starts_with("refs/") || validate_reference_name(symbolic).is_err() {
            return Err(LocalGitFailure::Operation);
        }
        return Ok(PinnedReferenceValue::Symbolic(symbolic.to_owned()));
    }
    let direct = std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| parse_full_object_id(value, authority.object_format))
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

#[cfg(test)]
pub(super) fn read_pinned_reference_with_test_hook<Hook: FnOnce()>(
    authority: &PinnedRepository,
    name: &str,
    after_metadata: Hook,
) -> Result<PinnedReferenceValue, LocalGitFailure> {
    read_pinned_reference_with_hooks(authority, name, after_metadata, || {}, || {})
}

#[cfg(test)]
pub(super) fn read_pinned_reference_with_post_read_test_hook<Hook: FnOnce()>(
    authority: &PinnedRepository,
    name: &str,
    after_first_read: Hook,
) -> Result<PinnedReferenceValue, LocalGitFailure> {
    read_pinned_reference_with_hooks(authority, name, || {}, after_first_read, || {})
}

#[cfg(test)]
pub(super) fn read_pinned_reference_with_post_confirmation_test_hook<Hook: FnOnce()>(
    authority: &PinnedRepository,
    name: &str,
    after_confirmation: Hook,
) -> Result<PinnedReferenceValue, LocalGitFailure> {
    read_pinned_reference_with_hooks(authority, name, || {}, || {}, after_confirmation)
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
    resolve_pinned_reference_chain_from_with_hook(authority, start, locks, || {})
}

fn resolve_pinned_reference_chain_from_with_hook<AfterFirstRead: FnOnce()>(
    authority: &PinnedRepository,
    start: &str,
    locks: Option<&[ReferenceLock]>,
    after_first_read: AfterFirstRead,
) -> Result<(Vec<String>, Option<git2::Oid>), LocalGitFailure> {
    const MAX_SYMBOLIC_REFERENCE_DEPTH: usize = 16;
    let operation_guard = locks
        .is_none()
        .then(|| authority.operation_guard())
        .transpose()?;
    let mut after_first_read = Some(after_first_read);
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
        if names.is_empty() {
            after_first_read.take().ok_or(LocalGitFailure::Operation)?();
        }
        if let Some(operation_guard) = &operation_guard {
            operation_guard.validate_supported_layout()?;
        }
        names.push(current);
        match value {
            PinnedReferenceValue::Direct(oid) => return Ok((names, Some(oid))),
            PinnedReferenceValue::Symbolic(target) => current = target,
            PinnedReferenceValue::Missing => return Ok((names, None)),
        }
    }
}

#[cfg(test)]
pub(super) fn resolve_pinned_reference_chain_with_test_hook<AfterFirstRead: FnOnce()>(
    authority: &PinnedRepository,
    after_first_read: AfterFirstRead,
) -> Result<(Vec<String>, Option<git2::Oid>), LocalGitFailure> {
    resolve_pinned_reference_chain_from_with_hook(authority, "HEAD", None, after_first_read)
}
