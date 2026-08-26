use std::{
    ffi::OsStr,
    fs,
    io::{Read, Seek, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
};

use bstr::BStr;
use git2::{Config, ObjectFormat};
use rustix::{
    fs::{CWD, Dir, Mode, OFlags, openat},
    io::dup,
};
use signalbox_tools_workspace::WorkspaceRootIdentity;

use crate::construction::LocalGitToolsConstructionError;
use crate::descriptor::{
    FileSnapshotIdentity, RepositoryIdentity, descriptor_path, file_identity,
    file_snapshot_identity, unsupported_control_files_are_absent,
};
use crate::failure::LocalGitFailure;
use crate::limits::{
    MAX_PACKED_REFS_BYTES, MAX_REFERENCE_BYTES, MAX_REPOSITORY_CONFIG_BYTES,
    MAX_REPOSITORY_INSPECTIONS, MAX_REVISION_BYTES, MAX_SHALLOW_BYTES, MAX_SHALLOW_ENTRIES,
};

pub(super) struct RepositoryConfig {
    pub(super) source: fs::File,
    pub(super) snapshot: fs::File,
    pub(super) identity: FileSnapshotIdentity,
    pub(super) object_format: ObjectFormat,
}

pub(super) struct RepositoryHead {
    pub(super) source: fs::File,
    pub(super) identity: FileSnapshotIdentity,
    pub(super) bytes: Vec<u8>,
}

pub(super) fn validate_repository_layout(
    root: &Path,
    injected_root: WorkspaceRootIdentity,
) -> Result<RepositoryIdentity, LocalGitToolsConstructionError> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let root_identity = file_identity(&root_metadata);
    if root_identity.device != injected_root.device || root_identity.inode != injected_root.inode {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let dot_git = root.join(".git");
    let metadata =
        fs::symlink_metadata(&dot_git).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let git_directory_identity = file_identity(&metadata);
    let git_directory = fs::File::from(
        openat(
            CWD,
            &dot_git,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitToolsConstructionError::Repository)?,
    );
    unsupported_control_files_are_absent(git_directory.as_fd())
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let config = open_repository_config_at(&git_directory)?;
    let head = open_repository_head_at(&git_directory, config.object_format)?;
    let refs = open_repository_refs_at(&git_directory)?;
    reject_administrative_symlinks_for_format(&git_directory, config.object_format)?;
    let config_metadata = config
        .source
        .metadata()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    Ok(RepositoryIdentity {
        root: root_identity,
        git_directory: git_directory_identity,
        refs: file_identity(
            &refs
                .metadata()
                .map_err(|_| LocalGitToolsConstructionError::Repository)?,
        ),
        config: file_identity(&config_metadata),
        head: head.identity.file,
    })
}

pub(super) fn open_repository_refs_at(
    git_directory: &fs::File,
) -> Result<fs::File, LocalGitToolsConstructionError> {
    openat(
        git_directory,
        "refs",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(|_| LocalGitToolsConstructionError::Repository)
}

pub(super) fn open_repository_head_at(
    git_directory: &fs::File,
    object_format: ObjectFormat,
) -> Result<RepositoryHead, LocalGitToolsConstructionError> {
    let descriptor = openat(
        git_directory,
        "HEAD",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let mut source = fs::File::from(descriptor);
    let before = source
        .metadata()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if !before.is_file() || before.len() > MAX_REVISION_BYTES as u64 {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    Read::by_ref(&mut source)
        .take((MAX_REVISION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let after = source
        .metadata()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let identity = file_snapshot_identity(&after);
    if bytes.len() > MAX_REVISION_BYTES
        || bytes.len() as u64 != before.len()
        || file_snapshot_identity(&before) != identity
        || !valid_head_record(&bytes, object_format)
    {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let path_descriptor = openat(
        git_directory,
        "HEAD",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let path_identity = file_snapshot_identity(
        &fs::File::from(path_descriptor)
            .metadata()
            .map_err(|_| LocalGitToolsConstructionError::Repository)?,
    );
    if path_identity != identity {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    Ok(RepositoryHead {
        source,
        identity,
        bytes,
    })
}

fn valid_head_record(bytes: &[u8], object_format: ObjectFormat) -> bool {
    let record = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if record.is_empty() || record.contains(&b'\n') || record.contains(&b'\r') {
        return false;
    }
    if let Some(target) = record.strip_prefix(b"ref: ") {
        return target.len() <= MAX_REFERENCE_BYTES
            && target.starts_with(b"refs/")
            && std::str::from_utf8(target).is_ok()
            && valid_reference_name(target);
    }
    parse_full_object_id_bytes(record, object_format).is_some()
}

pub(super) fn reject_administrative_symlinks(
    git_directory: &OwnedFd,
    object_format: ObjectFormat,
) -> Result<(), LocalGitToolsConstructionError> {
    reject_administrative_symlinks_for_format(
        &fs::File::from(
            dup(git_directory).map_err(|_| LocalGitToolsConstructionError::Repository)?,
        ),
        object_format,
    )
}

pub(super) const fn object_id_bytes(object_format: ObjectFormat) -> usize {
    match object_format {
        ObjectFormat::Sha1 => 20,
        ObjectFormat::Sha256 => 32,
    }
}

pub(super) fn parse_full_object_id(value: &str, object_format: ObjectFormat) -> Option<git2::Oid> {
    parse_full_object_id_bytes(value.as_bytes(), object_format)
}

pub(super) fn parse_full_object_id_bytes(
    value: &[u8],
    object_format: ObjectFormat,
) -> Option<git2::Oid> {
    let parsed = gix_hash::ObjectId::from_hex(value).ok()?;
    let expected_kind = match object_format {
        ObjectFormat::Sha1 => gix_hash::Kind::Sha1,
        ObjectFormat::Sha256 => gix_hash::Kind::Sha256,
    };
    (parsed.kind() == expected_kind)
        .then(|| git2::Oid::from_bytes(parsed.as_slice()).ok())
        .flatten()
}

pub(super) fn valid_reference_name(name: &[u8]) -> bool {
    gix_validate::reference::name(BStr::new(name)).is_ok()
}

fn reject_administrative_symlinks_for_format(
    git_directory: &fs::File,
    object_format: ObjectFormat,
) -> Result<(), LocalGitToolsConstructionError> {
    reject_administrative_symlinks_for_format_with_observer(git_directory, object_format, |_| {})
}

#[cfg(test)]
pub(super) fn reject_administrative_symlinks_with_test_observer<Observer>(
    git_directory: &OwnedFd,
    object_format: ObjectFormat,
    observer: Observer,
) -> Result<(), LocalGitToolsConstructionError>
where
    Observer: FnMut(usize),
{
    let git_directory =
        fs::File::from(dup(git_directory).map_err(|_| LocalGitToolsConstructionError::Repository)?);
    reject_administrative_symlinks_for_format_with_observer(&git_directory, object_format, observer)
}

fn reject_administrative_symlinks_for_format_with_observer<Observer>(
    git_directory: &fs::File,
    object_format: ObjectFormat,
    mut observer: Observer,
) -> Result<(), LocalGitToolsConstructionError>
where
    Observer: FnMut(usize),
{
    let root = dup(git_directory).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let root_entries =
        Dir::read_from(&root).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let mut pending = vec![(root, root_entries, AdministrativeDirectoryKind::Root)];
    let mut inspected = 0_usize;
    observer(pending.len());
    while let Some((current, entries, directory_kind)) = pending.last_mut() {
        let directory_kind = *directory_kind;
        match entries.read() {
            Some(entry) => {
                let entry = entry.map_err(|_| LocalGitToolsConstructionError::Repository)?;
                let name = OsStr::from_bytes(entry.file_name().to_bytes());
                if name == OsStr::new(".") || name == OsStr::new("..") {
                    continue;
                }
                inspected = inspected.saturating_add(1);
                if inspected > MAX_REPOSITORY_INSPECTIONS {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
                if directory_kind == AdministrativeDirectoryKind::References
                    && name.to_str().is_none()
                {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
                match openat(
                    &current,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(directory) => {
                        let child_kind = match (directory_kind, name) {
                            (AdministrativeDirectoryKind::Root, name)
                                if name == OsStr::new("refs") =>
                            {
                                AdministrativeDirectoryKind::References
                            }
                            (AdministrativeDirectoryKind::References, _) => {
                                AdministrativeDirectoryKind::References
                            }
                            _ => AdministrativeDirectoryKind::Other,
                        };
                        let entries = Dir::read_from(&directory)
                            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
                        pending.push((directory, entries, child_kind));
                        observer(pending.len());
                        continue;
                    }
                    Err(error)
                        if error == rustix::io::Errno::NOTDIR
                            && directory_kind == AdministrativeDirectoryKind::Root
                            && name == OsStr::new("refs") =>
                    {
                        return Err(LocalGitToolsConstructionError::Repository);
                    }
                    Err(error) if error == rustix::io::Errno::NOTDIR => {}
                    Err(_) => return Err(LocalGitToolsConstructionError::Repository),
                }
                let descriptor = openat(
                    &current,
                    name,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| LocalGitToolsConstructionError::Repository)?;
                let mut file = fs::File::from(descriptor);
                let metadata = file
                    .metadata()
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?;
                if !metadata.is_file() {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
                let limit = match (directory_kind, name) {
                    (AdministrativeDirectoryKind::Root, name) if name == OsStr::new("HEAD") => {
                        Some(MAX_REVISION_BYTES)
                    }
                    (AdministrativeDirectoryKind::References, _) => Some(MAX_REVISION_BYTES),
                    (AdministrativeDirectoryKind::Root, name)
                        if name == OsStr::new("packed-refs") =>
                    {
                        Some(MAX_PACKED_REFS_BYTES)
                    }
                    (AdministrativeDirectoryKind::Root, name) if name == OsStr::new("shallow") => {
                        Some(MAX_SHALLOW_BYTES)
                    }
                    _ => None,
                };
                if limit.is_some_and(|limit| metadata.len() > limit as u64) {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
                if directory_kind == AdministrativeDirectoryKind::Root
                    && name == OsStr::new("shallow")
                {
                    validate_shallow_file_at(current, name, &mut file, object_format)?;
                }
                if directory_kind == AdministrativeDirectoryKind::Root
                    && name == OsStr::new("packed-refs")
                {
                    validate_packed_reference_name_encoding(&mut file)?;
                }
            }
            None => {
                pending.pop();
            }
        }
    }
    Ok(())
}

fn validate_packed_reference_name_encoding(
    file: &mut fs::File,
) -> Result<(), LocalGitToolsConstructionError> {
    let mut bytes = Vec::with_capacity(MAX_PACKED_REFS_BYTES);
    Read::by_ref(file)
        .take((MAX_PACKED_REFS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if bytes.len() > MAX_PACKED_REFS_BYTES {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() || line.starts_with(b"#") || line.starts_with(b"^") {
            continue;
        }
        let separator = line
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or(LocalGitToolsConstructionError::Repository)?;
        let name = line
            .get(separator + 1..)
            .ok_or(LocalGitToolsConstructionError::Repository)?;
        if name.len() > MAX_REFERENCE_BYTES || std::str::from_utf8(name).is_err() {
            return Err(LocalGitToolsConstructionError::Repository);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AdministrativeDirectoryKind {
    Root,
    References,
    Other,
}

pub(super) fn validate_shallow_file(
    file: &mut fs::File,
    object_format: ObjectFormat,
) -> Result<(), LocalGitToolsConstructionError> {
    let mut bytes = Vec::with_capacity(MAX_SHALLOW_BYTES);
    Read::by_ref(file)
        .take((MAX_SHALLOW_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if bytes.len() > MAX_SHALLOW_BYTES {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    if bytes.is_empty() {
        return Ok(());
    }
    let records = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    if records.is_empty() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let mut entries = 0_usize;
    for line in records.split(|byte| *byte == b'\n') {
        entries = entries.saturating_add(1);
        if entries > MAX_SHALLOW_ENTRIES
            || line.is_empty()
            || parse_full_object_id_bytes(line, object_format).is_none()
        {
            return Err(LocalGitToolsConstructionError::Repository);
        }
    }
    // The private repository cannot represent a live shallow boundary without
    // reopening it by path. Reject nonempty shallow repositories instead of
    // silently exposing parents hidden by workspace Git semantics.
    Err(LocalGitToolsConstructionError::Repository)
}

pub(super) fn validate_live_shallow(
    git_directory: &fs::File,
    object_format: ObjectFormat,
) -> Result<(), LocalGitFailure> {
    let descriptor = match openat(
        git_directory,
        "shallow",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(()),
        Err(_) => return Err(LocalGitFailure::Repository),
    };
    let mut file = fs::File::from(descriptor);
    validate_shallow_file_at(
        &dup(git_directory).map_err(|_| LocalGitFailure::Repository)?,
        OsStr::new("shallow"),
        &mut file,
        object_format,
    )
    .map_err(|_| LocalGitFailure::Repository)
}

fn validate_shallow_file_at(
    parent: &OwnedFd,
    name: &OsStr,
    file: &mut fs::File,
    object_format: ObjectFormat,
) -> Result<(), LocalGitToolsConstructionError> {
    validate_shallow_file_at_with_hook(parent, name, file, object_format, || {})
}

#[cfg(test)]
pub(super) fn validate_shallow_file_at_with_test_hook<AfterRead: FnOnce()>(
    parent: &OwnedFd,
    name: &OsStr,
    file: &mut fs::File,
    object_format: ObjectFormat,
    after_read: AfterRead,
) -> Result<(), LocalGitToolsConstructionError> {
    validate_shallow_file_at_with_hook(parent, name, file, object_format, after_read)
}

fn validate_shallow_file_at_with_hook<AfterRead: FnOnce()>(
    parent: &OwnedFd,
    name: &OsStr,
    file: &mut fs::File,
    object_format: ObjectFormat,
    after_read: AfterRead,
) -> Result<(), LocalGitToolsConstructionError> {
    let before = file
        .metadata()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    validate_shallow_file(file, object_format)?;
    let after = file
        .metadata()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if file_snapshot_identity(&before) != file_snapshot_identity(&after) {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    after_read();
    let path_descriptor = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let path_metadata = fs::File::from(path_descriptor)
        .metadata()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if file_snapshot_identity(&after) != file_snapshot_identity(&path_metadata) {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    Ok(())
}

pub(super) fn open_repository_config_at(
    git_directory: &fs::File,
) -> Result<RepositoryConfig, LocalGitToolsConstructionError> {
    let descriptor = openat(
        git_directory,
        "config",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    validate_repository_config_descriptor(descriptor)
}

fn validate_repository_config_descriptor(
    descriptor: OwnedFd,
) -> Result<RepositoryConfig, LocalGitToolsConstructionError> {
    let mut file = fs::File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if !metadata.is_file() || metadata.len() > MAX_REPOSITORY_CONFIG_BYTES as u64 {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_REPOSITORY_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let after_read = file
        .metadata()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if bytes.len() > MAX_REPOSITORY_CONFIG_BYTES
        || bytes.len() as u64 != metadata.len()
        || file_snapshot_identity(&metadata) != file_snapshot_identity(&after_read)
    {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    if bytes.starts_with(b"\xef\xbb\xbf") {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let config =
        String::from_utf8(bytes.clone()).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let mut section = "";
    let mut object_format_seen = false;
    let mut repository_format_version_seen = false;
    let mut bare_seen = false;
    for line in config.lines() {
        let mut normalized = line.trim().to_owned();
        if normalized.is_empty() || normalized.starts_with('#') || normalized.starts_with(';') {
            continue;
        }
        if normalized.starts_with('[') {
            let closing = normalized
                .find(']')
                .ok_or(LocalGitToolsConstructionError::Repository)?;
            let header = normalized[1..closing].trim().to_ascii_lowercase();
            let section_name = header
                .split(|character: char| character.is_ascii_whitespace() || character == '.')
                .next()
                .unwrap_or("");
            section = if header == "core" {
                "core"
            } else if header == "extensions" {
                "extensions"
            } else if matches!(section_name, "filter" | "include" | "includeif") {
                return Err(LocalGitToolsConstructionError::Repository);
            } else {
                ""
            };
            let trailing = normalized[closing + 1..].trim();
            if trailing.is_empty() || trailing.starts_with('#') || trailing.starts_with(';') {
                continue;
            }
            normalized = trailing.to_owned();
        }
        if section == "core" {
            let key_value = normalized.split_once('=');
            let key = key_value
                .map(|(key, _)| key.trim())
                .or_else(|| normalized.split_ascii_whitespace().next())
                .unwrap_or("")
                .to_ascii_lowercase();
            if key_value.is_none() && matches!(key.as_str(), "bare" | "repositoryformatversion") {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            if matches!(
                key.as_str(),
                "worktree" | "excludesfile" | "attributesfile" | "hookspath" | "fsmonitor"
            ) {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            if let Some((key, _value)) = key_value
                && key.trim().eq_ignore_ascii_case("repositoryformatversion")
            {
                if repository_format_version_seen {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
                repository_format_version_seen = true;
            }
            if let Some((key, _value)) = key_value
                && key.trim().eq_ignore_ascii_case("bare")
            {
                if bare_seen {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
                bare_seen = true;
            }
        }
        if section == "extensions" {
            let (key, _value) = normalized
                .split_once('=')
                .ok_or(LocalGitToolsConstructionError::Repository)?;
            match key.trim().to_ascii_lowercase().as_str() {
                "objectformat" if !object_format_seen => {
                    object_format_seen = true;
                }
                _ => {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
            }
        }
    }
    let mut snapshot =
        tempfile::tempfile().map_err(|_| LocalGitToolsConstructionError::Repository)?;
    snapshot
        .write_all(&bytes)
        .and_then(|()| snapshot.rewind())
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let parsed = Config::open(&descriptor_path(&snapshot))
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let repository_format_version = if repository_format_version_seen {
        parsed
            .get_i64("core.repositoryformatversion")
            .ok()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(LocalGitToolsConstructionError::Repository)?
    } else {
        0
    };
    if bare_seen
        && parsed
            .get_bool("core.bare")
            .ok()
            .filter(|value| !value)
            .is_none()
    {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let object_format = if object_format_seen {
        match parsed
            .get_string("extensions.objectformat")
            .map_err(|_| LocalGitToolsConstructionError::Repository)?
            .as_str()
        {
            "sha1" => ObjectFormat::Sha1,
            "sha256" => ObjectFormat::Sha256,
            _ => return Err(LocalGitToolsConstructionError::Repository),
        }
    } else {
        ObjectFormat::Sha1
    };
    match (repository_format_version, object_format_seen) {
        (0, false) | (1, _) => {}
        _ => return Err(LocalGitToolsConstructionError::Repository),
    }
    snapshot
        .rewind()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    Ok(RepositoryConfig {
        source: file,
        snapshot,
        identity: file_snapshot_identity(&metadata),
        object_format,
    })
}
