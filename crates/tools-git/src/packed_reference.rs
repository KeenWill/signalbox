use std::{fs, io::Read};

use rustix::fs::{Mode, OFlags, openat};

use crate::failure::LocalGitFailure;
use crate::limits::MAX_PACKED_REFS_BYTES;
use crate::pinning::PinnedRepository;

pub(super) fn packed_reference_exists(
    authority: &PinnedRepository,
    reference_name: &str,
) -> Result<bool, LocalGitFailure> {
    for (_, existing) in read_packed_references(authority)? {
        let requested = reference_name.as_bytes();
        let requested_prefix = [requested, b"/"].concat();
        let existing_prefix = [existing.as_slice(), b"/"].concat();
        if existing == requested
            || existing.starts_with(&requested_prefix)
            || requested.starts_with(&existing_prefix)
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
        let requested_prefix = [requested, b"/"].concat();
        let existing_prefix = [existing.as_slice(), b"/"].concat();
        if existing != requested
            && (existing.starts_with(&requested_prefix) || requested.starts_with(&existing_prefix))
        {
            return Ok(true);
        }
    }
    Ok(false)
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
    if bytes.len() > MAX_PACKED_REFS_BYTES {
        return Err(LocalGitFailure::Operation);
    }
    let mut references = Vec::new();
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
            .and_then(|oid| git2::Oid::from_str(oid).ok())
            .ok_or(LocalGitFailure::Operation)?;
        let existing = line
            .get(separator + 1..)
            .ok_or(LocalGitFailure::Operation)?;
        if existing.is_empty()
            || std::str::from_utf8(existing)
                .ok()
                .is_none_or(|name| !git2::Reference::is_valid_name(name))
        {
            return Err(LocalGitFailure::Operation);
        }
        references.push((oid, existing.to_vec()));
    }
    Ok(references)
}
