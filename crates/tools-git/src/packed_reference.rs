use std::{collections::HashSet, fs, io::Read};

use rustix::fs::{Mode, OFlags, openat};

use crate::descriptor::file_snapshot_identity;
use crate::failure::LocalGitFailure;
use crate::layout::{parse_full_object_id, valid_reference_name};
use crate::limits::MAX_PACKED_REFS_BYTES;
use crate::pinning::PinnedRepository;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PackedReferenceState {
    pub(super) target: Option<git2::Oid>,
    pub(super) namespace: PackedReferenceNamespace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PackedReferenceNamespace {
    Clear,
    Conflicts,
}

pub(super) fn packed_reference_exists(
    authority: &PinnedRepository,
    reference_name: &str,
) -> Result<bool, LocalGitFailure> {
    for (_, existing) in read_packed_references(authority)? {
        let requested = reference_name.as_bytes();
        if existing == requested
            || is_namespaced_under(&existing, requested)
            || is_namespaced_under(requested, &existing)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn packed_reference_namespace_conflicts(
    authority: &PinnedRepository,
    reference_name: &str,
) -> Result<bool, LocalGitFailure> {
    for (_, existing) in read_packed_references(authority)? {
        let requested = reference_name.as_bytes();
        if existing != requested
            && (is_namespaced_under(&existing, requested)
                || is_namespaced_under(requested, &existing))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn packed_reference_state(
    authority: &PinnedRepository,
    reference_name: &str,
) -> Result<PackedReferenceState, LocalGitFailure> {
    let requested = reference_name.as_bytes();
    let mut target = None;
    let mut namespace = PackedReferenceNamespace::Clear;
    for (oid, existing) in read_packed_references(authority)? {
        if existing == requested {
            target = Some(oid);
        } else if is_namespaced_under(&existing, requested)
            || is_namespaced_under(requested, &existing)
        {
            namespace = PackedReferenceNamespace::Conflicts;
        }
    }
    Ok(PackedReferenceState { target, namespace })
}

fn is_namespaced_under(candidate: &[u8], namespace: &[u8]) -> bool {
    candidate.len() > namespace.len()
        && candidate.starts_with(namespace)
        && candidate[namespace.len()] == b'/'
}

pub(super) fn packed_reference_target(
    authority: &PinnedRepository,
    reference_name: &str,
) -> Result<Option<git2::Oid>, LocalGitFailure> {
    Ok(read_packed_references(authority)?
        .into_iter()
        .find_map(|(oid, name)| (name == reference_name.as_bytes()).then_some(oid)))
}

pub(super) fn read_packed_references(
    authority: &PinnedRepository,
) -> Result<Vec<(git2::Oid, Vec<u8>)>, LocalGitFailure> {
    read_packed_references_with_hook(authority, || {})
}

#[cfg(test)]
pub(super) fn read_packed_references_with_test_hook<AfterRead: FnOnce()>(
    authority: &PinnedRepository,
    after_snapshot: AfterRead,
) -> Result<Vec<(git2::Oid, Vec<u8>)>, LocalGitFailure> {
    read_packed_references_with_hook(authority, after_snapshot)
}

fn read_packed_references_with_hook<AfterRead: FnOnce()>(
    authority: &PinnedRepository,
    after_snapshot: AfterRead,
) -> Result<Vec<(git2::Oid, Vec<u8>)>, LocalGitFailure> {
    authority.validate_supported_layout()?;
    let descriptor = match openat(
        &authority.git_directory,
        "packed-refs",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => {
            authority.validate_supported_layout()?;
            match openat(
                &authority.git_directory,
                "packed-refs",
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Err(error) if error == rustix::io::Errno::NOENT => {}
                Ok(_) | Err(_) => return Err(LocalGitFailure::Operation),
            }
            return Ok(Vec::new());
        }
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    let mut file = fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if !metadata.is_file() || metadata.len() > MAX_PACKED_REFS_BYTES as u64 {
        return Err(LocalGitFailure::Operation);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_PACKED_REFS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    let after_read = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if bytes.len() > MAX_PACKED_REFS_BYTES
        || bytes.len() as u64 != metadata.len()
        || file_snapshot_identity(&metadata) != file_snapshot_identity(&after_read)
    {
        return Err(LocalGitFailure::Operation);
    }
    let snapshot = file_snapshot_identity(&after_read);
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(LocalGitFailure::Operation);
    }
    if bytes.is_empty() {
        after_snapshot();
        authority.validate_supported_layout()?;
        validate_packed_reference_path(authority, &file, snapshot)?;
        return Ok(Vec::new());
    }
    let records = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let mut references: Vec<(git2::Oid, Vec<u8>)> = Vec::new();
    let mut names = HashSet::new();
    let mut previous_was_reference = false;
    let mut header_seen = false;
    let mut sorted = false;
    for line in records.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            return Err(LocalGitFailure::Operation);
        }
        if let Some(traits) = line.strip_prefix(b"# pack-refs with:") {
            if header_seen || !references.is_empty() || previous_was_reference {
                return Err(LocalGitFailure::Operation);
            }
            let traits = match traits {
                b"" => b"".as_slice(),
                traits => traits
                    .strip_prefix(b" ")
                    .ok_or(LocalGitFailure::Operation)?,
            };
            sorted = traits
                .split(|byte| *byte == b' ')
                .any(|trait_name| trait_name == b"sorted");
            header_seen = true;
            previous_was_reference = false;
            continue;
        }
        if matches!(line.first(), Some(b'#')) {
            return Err(LocalGitFailure::Operation);
        }
        if let Some(peeled) = line.strip_prefix(b"^") {
            if !previous_was_reference
                || std::str::from_utf8(peeled)
                    .ok()
                    .and_then(|oid| parse_full_object_id(oid, authority.object_format))
                    .is_none()
            {
                return Err(LocalGitFailure::Operation);
            }
            previous_was_reference = false;
            continue;
        }
        let separator = line
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or(LocalGitFailure::Operation)?;
        let oid = std::str::from_utf8(&line[..separator])
            .ok()
            .and_then(|oid| parse_full_object_id(oid, authority.object_format))
            .ok_or(LocalGitFailure::Operation)?;
        let existing = line
            .get(separator + 1..)
            .ok_or(LocalGitFailure::Operation)?;
        if existing.is_empty()
            || !existing.starts_with(b"refs/")
            || std::str::from_utf8(existing).is_err()
            || !valid_reference_name(existing)
            || !names.insert(existing.to_vec())
            || (sorted
                && references
                    .last()
                    .is_some_and(|(_, previous)| previous.as_slice() >= existing))
        {
            return Err(LocalGitFailure::Operation);
        }
        references.push((oid, existing.to_vec()));
        previous_was_reference = true;
    }
    after_snapshot();
    authority.validate_supported_layout()?;
    validate_packed_reference_path(authority, &file, snapshot)?;
    Ok(references)
}

fn validate_packed_reference_path(
    authority: &PinnedRepository,
    file: &fs::File,
    expected: crate::descriptor::FileSnapshotIdentity,
) -> Result<(), LocalGitFailure> {
    let descriptor_metadata = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    let path_descriptor = openat(
        &authority.git_directory,
        "packed-refs",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let path_metadata = fs::File::from(path_descriptor)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    if file_snapshot_identity(&descriptor_metadata) != expected
        || file_snapshot_identity(&path_metadata) != expected
    {
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}
