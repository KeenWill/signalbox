use std::{collections::HashSet, fs, io::Read};

use rustix::fs::{Mode, OFlags, openat};

use crate::descriptor::file_snapshot_identity;
use crate::failure::LocalGitFailure;
use crate::limits::MAX_PACKED_REFS_BYTES;
use crate::pinning::PinnedRepository;

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
    let descriptor = match openat(
        &authority.git_directory,
        "packed-refs",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(Vec::new()),
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
    let mut references = Vec::new();
    let mut names = HashSet::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() || matches!(line.first(), Some(b'#' | b'^')) {
            continue;
        }
        let separator = line
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or(LocalGitFailure::Operation)?;
        let oid = std::str::from_utf8(&line[..separator])
            .ok()
            .and_then(|oid| git2::Oid::from_str_ext(oid, authority.object_format).ok())
            .ok_or(LocalGitFailure::Operation)?;
        let existing = line
            .get(separator + 1..)
            .ok_or(LocalGitFailure::Operation)?;
        if existing.is_empty()
            || std::str::from_utf8(existing)
                .ok()
                .is_none_or(|name| !git2::Reference::is_valid_name(name))
            || !names.insert(existing.to_vec())
        {
            return Err(LocalGitFailure::Operation);
        }
        references.push((oid, existing.to_vec()));
    }
    Ok(references)
}
