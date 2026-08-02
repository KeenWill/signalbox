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

use git2::ObjectFormat;
use rustix::{
    fs::{CWD, Dir, Mode, OFlags, openat},
    io::dup,
};

use crate::construction::LocalGitToolsConstructionError;
use crate::descriptor::{
    RepositoryIdentity, file_identity, file_snapshot_identity, unsupported_control_files_are_absent,
};
use crate::limits::{
    MAX_PACKED_REFS_BYTES, MAX_REPOSITORY_CONFIG_BYTES, MAX_REPOSITORY_INSPECTIONS,
    MAX_REVISION_BYTES, MAX_SHALLOW_BYTES, MAX_SHALLOW_ENTRIES,
};

pub(super) struct RepositoryConfig {
    pub(super) source: fs::File,
    pub(super) snapshot: fs::File,
    pub(super) object_format: ObjectFormat,
}

pub(super) fn validate_repository_layout(
    root: &Path,
) -> Result<RepositoryIdentity, LocalGitToolsConstructionError> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let root_identity = file_identity(&root_metadata);
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
    reject_administrative_symlinks_for_format(&git_directory, config.object_format)?;
    let config_metadata = config
        .source
        .metadata()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    Ok(RepositoryIdentity {
        root: root_identity,
        git_directory: git_directory_identity,
        config: file_identity(&config_metadata),
    })
}

pub(super) fn reject_administrative_symlinks(
    git_directory: &OwnedFd,
) -> Result<(), LocalGitToolsConstructionError> {
    reject_administrative_symlinks_for_format(
        &fs::File::from(
            dup(git_directory).map_err(|_| LocalGitToolsConstructionError::Repository)?,
        ),
        ObjectFormat::Sha1,
    )
}

fn reject_administrative_symlinks_for_format(
    git_directory: &fs::File,
    object_format: ObjectFormat,
) -> Result<(), LocalGitToolsConstructionError> {
    let root = dup(git_directory).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let mut pending = vec![(root, AdministrativeDirectoryKind::Root)];
    let mut inspected = 0_usize;
    while let Some((current, directory_kind)) = pending.pop() {
        let mut entries =
            Dir::read_from(&current).map_err(|_| LocalGitToolsConstructionError::Repository)?;
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(|_| LocalGitToolsConstructionError::Repository)?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == OsStr::new(".") || name == OsStr::new("..") {
                continue;
            }
            inspected = inspected.saturating_add(1);
            if inspected > MAX_REPOSITORY_INSPECTIONS {
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
                        (AdministrativeDirectoryKind::Root, name) if name == OsStr::new("refs") => {
                            AdministrativeDirectoryKind::References
                        }
                        (AdministrativeDirectoryKind::References, _) => {
                            AdministrativeDirectoryKind::References
                        }
                        _ => AdministrativeDirectoryKind::Other,
                    };
                    pending.push((directory, child_kind));
                    continue;
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
                (AdministrativeDirectoryKind::Root, name)
                    if name == OsStr::new("HEAD") || name == OsStr::new("refs") =>
                {
                    Some(MAX_REVISION_BYTES)
                }
                (AdministrativeDirectoryKind::References, _) => Some(MAX_REVISION_BYTES),
                (AdministrativeDirectoryKind::Root, name) if name == OsStr::new("packed-refs") => {
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
            if directory_kind == AdministrativeDirectoryKind::Root && name == OsStr::new("shallow")
            {
                validate_shallow_file(&mut file, object_format)?;
            }
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
    let mut entries = 0_usize;
    let object_id_bytes = match object_format {
        ObjectFormat::Sha1 => 40,
        ObjectFormat::Sha256 => 64,
    };
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        entries = entries.saturating_add(1);
        if entries > MAX_SHALLOW_ENTRIES
            || line.len() != object_id_bytes
            || !line.iter().all(u8::is_ascii_hexdigit)
        {
            return Err(LocalGitToolsConstructionError::Repository);
        }
    }
    Ok(())
}

pub(super) fn reject_escaping_config(
    config_path: &Path,
) -> Result<(), LocalGitToolsConstructionError> {
    open_repository_config(config_path).map(drop)
}

pub(super) fn open_repository_config(
    config_path: &Path,
) -> Result<RepositoryConfig, LocalGitToolsConstructionError> {
    let descriptor = openat(
        CWD,
        config_path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    validate_repository_config_descriptor(descriptor)
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
    let mut object_format = ObjectFormat::Sha1;
    let mut object_format_seen = false;
    let mut repository_format_version = None;
    let mut bare_seen = false;
    for line in config.lines() {
        let mut normalized = line.trim().to_ascii_lowercase();
        if normalized.is_empty() || normalized.starts_with('#') || normalized.starts_with(';') {
            continue;
        }
        if normalized.starts_with('[') {
            let closing = normalized
                .find(']')
                .ok_or(LocalGitToolsConstructionError::Repository)?;
            let header = normalized[1..closing].trim();
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
            let bare_without_value = key_value.is_none()
                && normalized
                    .split_ascii_whitespace()
                    .next()
                    .is_some_and(|key| key == "bare");
            if bare_without_value {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            if key_value.is_none()
                && normalized
                    .split_ascii_whitespace()
                    .next()
                    .is_some_and(|key| key == "repositoryformatversion")
            {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            let file_valued = key_value.is_some_and(|(key, _)| {
                matches!(
                    key.trim(),
                    "worktree" | "excludesfile" | "attributesfile" | "hookspath" | "fsmonitor"
                )
            });
            if file_valued {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            if let Some((key, value)) = key_value
                && key.trim() == "repositoryformatversion"
            {
                if repository_format_version.is_some() {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
                repository_format_version = Some(
                    value
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| LocalGitToolsConstructionError::Repository)?,
                );
            }
            if let Some((key, value)) = key_value
                && key.trim() == "bare"
            {
                if bare_seen || !matches!(value.trim(), "false" | "no" | "off" | "0") {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
                bare_seen = true;
            }
        }
        if section == "extensions" {
            let (key, value) = normalized
                .split_once('=')
                .ok_or(LocalGitToolsConstructionError::Repository)?;
            match key.trim() {
                "objectformat" if !object_format_seen => {
                    object_format = match value.trim() {
                        "sha1" => ObjectFormat::Sha1,
                        "sha256" => ObjectFormat::Sha256,
                        _ => return Err(LocalGitToolsConstructionError::Repository),
                    };
                    object_format_seen = true;
                }
                _ => {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
            }
        }
    }
    match (repository_format_version.unwrap_or(0), object_format_seen) {
        (0, false) | (1, _) => {}
        _ => return Err(LocalGitToolsConstructionError::Repository),
    }
    let mut snapshot =
        tempfile::tempfile().map_err(|_| LocalGitToolsConstructionError::Repository)?;
    snapshot
        .write_all(&bytes)
        .and_then(|()| snapshot.sync_all())
        .and_then(|()| snapshot.rewind())
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    Ok(RepositoryConfig {
        source: file,
        snapshot,
        object_format,
    })
}
