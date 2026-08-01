use std::{
    ffi::OsStr,
    fs,
    io::{Read, Seek, Write},
    os::{fd::OwnedFd, unix::ffi::OsStrExt},
    path::{Path, PathBuf},
};

use rustix::{
    fs::{CWD, Dir, Mode, OFlags, openat},
    io::dup,
};

use crate::construction::LocalGitToolsConstructionError;
use crate::descriptor::{RepositoryIdentity, file_identity};
use crate::limits::{
    MAX_PACKED_REFS_BYTES, MAX_REPOSITORY_CONFIG_BYTES, MAX_REVISION_BYTES, MAX_SHALLOW_BYTES,
    MAX_SHALLOW_ENTRIES,
};

pub(super) struct RepositoryConfig {
    pub(super) source: fs::File,
    pub(super) snapshot: fs::File,
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
    let git_directory = openat(
        CWD,
        &dot_git,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if dot_git.join("commondir").exists() || dot_git.join("objects/info/alternates").exists() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    reject_administrative_symlinks(&git_directory)?;
    reject_escaping_config(&dot_git.join("config"))?;
    let config_metadata = fs::symlink_metadata(dot_git.join("config"))
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
    let root = dup(git_directory).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let mut pending = vec![(root, PathBuf::new())];
    let mut inspected = 0_usize;
    while let Some((current, relative_directory)) = pending.pop() {
        let mut entries =
            Dir::read_from(&current).map_err(|_| LocalGitToolsConstructionError::Repository)?;
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(|_| LocalGitToolsConstructionError::Repository)?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == OsStr::new(".") || name == OsStr::new("..") {
                continue;
            }
            inspected = inspected.saturating_add(1);
            if inspected > 100_000 {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            let mut relative = relative_directory.clone();
            relative.push(name);
            match openat(
                &current,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(directory) => {
                    pending.push((directory, relative));
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
            let limit = if relative == Path::new("HEAD") || relative.starts_with("refs") {
                Some(MAX_REVISION_BYTES)
            } else if relative == Path::new("packed-refs") {
                Some(MAX_PACKED_REFS_BYTES)
            } else if relative == Path::new("shallow") {
                Some(MAX_SHALLOW_BYTES)
            } else {
                None
            };
            if limit.is_some_and(|limit| metadata.len() > limit as u64) {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            if relative == Path::new("shallow") {
                validate_shallow_file(&mut file)?;
            }
        }
    }
    Ok(())
}

pub(super) fn validate_shallow_file(
    file: &mut fs::File,
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
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        entries = entries.saturating_add(1);
        if entries > MAX_SHALLOW_ENTRIES
            || line.len() != 40
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
    if bytes.len() > MAX_REPOSITORY_CONFIG_BYTES || bytes.len() as u64 != metadata.len() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    if bytes.starts_with(b"\xef\xbb\xbf") {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let config =
        String::from_utf8(bytes.clone()).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let mut section = "";
    for line in config.lines() {
        let mut normalized = line.trim().to_ascii_lowercase();
        if normalized.starts_with('[') {
            let closing = normalized
                .find(']')
                .ok_or(LocalGitToolsConstructionError::Repository)?;
            let header = &normalized[..=closing];
            section = if header.starts_with("[core]") {
                "core"
            } else if header.starts_with("[extensions]") {
                "extensions"
            } else if header.starts_with("[filter ") || header.starts_with("[include") {
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
            let file_valued = normalized.split_once('=').is_some_and(|(key, _)| {
                matches!(
                    key.trim(),
                    "worktree" | "excludesfile" | "attributesfile" | "hookspath" | "fsmonitor"
                )
            });
            if file_valued {
                return Err(LocalGitToolsConstructionError::Repository);
            }
        }
        if section == "extensions"
            && normalized
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "worktreeconfig")
        {
            return Err(LocalGitToolsConstructionError::Repository);
        }
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
    })
}
