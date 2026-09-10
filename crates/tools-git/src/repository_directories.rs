//! Descriptor-pinned Git administration reached through the configured root.

use crate::{
    descriptor::{FileIdentity, FileSnapshotIdentity, file_identity, file_snapshot_identity},
    failure::LocalGitFailure,
};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat};
use std::{ffi::OsStr, fs::File, io::Read, os::unix::ffi::OsStrExt, path::Path};

// Linux PATH_MAX plus the Git marker prefix and newline.
const MAX_ADMINISTRATION_MARKER_BYTES: usize = 4096 + b"gitdir: \n".len();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AdministrationBinding {
    pub(super) worktree: FileIdentity,
    pub(super) common: FileIdentity,
    marker: Option<FileSnapshotIdentity>,
    common_marker: Option<FileSnapshotIdentity>,
}

pub(super) struct AdministrationDirectories {
    pub(super) worktree: File,
    pub(super) common: File,
    pub(super) binding: AdministrationBinding,
}

impl AdministrationDirectories {
    pub(super) fn open(root: &File) -> Result<Self, LocalGitFailure> {
        let status = statat(root, ".git", AtFlags::SYMLINK_NOFOLLOW).map_err(rejected)?;
        let (worktree, marker) = match FileType::from_raw_mode(status.st_mode) {
            FileType::Directory => (open_directory(root, Path::new(".git"))?, None),
            FileType::RegularFile => {
                let (bytes, identity) = read_marker(root, ".git")?;
                let path = bytes
                    .strip_prefix(b"gitdir: ")
                    .ok_or(LocalGitFailure::Repository)?;
                (open_directory(root, marker_path(path)?)?, Some(identity))
            }
            _ => return Err(LocalGitFailure::Repository),
        };
        let (common, common_marker) =
            match statat(&worktree, "commondir", AtFlags::SYMLINK_NOFOLLOW) {
                Err(rustix::io::Errno::NOENT) => (worktree.try_clone().map_err(rejected)?, None),
                Ok(status)
                    if marker.is_some()
                        && FileType::from_raw_mode(status.st_mode) == FileType::RegularFile =>
                {
                    let (bytes, identity) = read_marker(&worktree, "commondir")?;
                    (
                        open_directory(&worktree, marker_path(&bytes)?)?,
                        Some(identity),
                    )
                }
                _ => return Err(LocalGitFailure::Repository),
            };
        let binding = AdministrationBinding {
            worktree: file_identity(&worktree.metadata().map_err(rejected)?),
            common: file_identity(&common.metadata().map_err(rejected)?),
            marker,
            common_marker,
        };
        Ok(Self {
            worktree,
            common,
            binding,
        })
    }
}

pub(super) fn validate_binding(
    root: &File,
    expected: AdministrationBinding,
) -> Result<(), LocalGitFailure> {
    if AdministrationDirectories::open(root)?.binding != expected {
        return Err(LocalGitFailure::Repository);
    }
    Ok(())
}

fn open_directory(parent: &File, path: &Path) -> Result<File, LocalGitFailure> {
    use std::path::Component;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = parent.try_clone().map_err(rejected)?;
    for component in path.components() {
        let name = match component {
            Component::RootDir => OsStr::new("/"),
            Component::ParentDir => OsStr::new(".."),
            Component::Normal(name) => name,
            Component::CurDir => continue,
            Component::Prefix(_) => return Err(LocalGitFailure::Repository),
        };
        directory = File::from(openat(&directory, name, flags, Mode::empty()).map_err(rejected)?);
    }
    Ok(directory)
}

fn marker_path(bytes: &[u8]) -> Result<&Path, LocalGitFailure> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
        return Err(LocalGitFailure::Repository);
    }
    Ok(Path::new(OsStr::from_bytes(bytes)))
}

fn read_marker(
    parent: &File,
    name: &str,
) -> Result<(Vec<u8>, FileSnapshotIdentity), LocalGitFailure> {
    let mut file = File::from(
        openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(rejected)?,
    );
    let before = file.metadata().map_err(rejected)?;
    if !before.is_file() || before.len() > MAX_ADMINISTRATION_MARKER_BYTES as u64 {
        return Err(LocalGitFailure::Repository);
    }
    let identity = file_snapshot_identity(&before);
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_ADMINISTRATION_MARKER_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(rejected)?;
    let current = File::from(
        openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(rejected)?,
    );
    if bytes.len() as u64 != identity.length
        || file_snapshot_identity(&file.metadata().map_err(rejected)?) != identity
        || file_snapshot_identity(&current.metadata().map_err(rejected)?) != identity
    {
        return Err(LocalGitFailure::Repository);
    }
    Ok((bytes, identity))
}

fn rejected<T>(_: T) -> LocalGitFailure {
    LocalGitFailure::Repository
}

/// Descriptor-pinned administration directories used by a configured repository.
#[derive(Debug)]
pub struct RepositoryAdministrationDirectories {
    /// Directory containing this worktree's HEAD and index.
    pub worktree: File,
    /// Directory containing shared references and objects.
    pub common: File,
}

/// Opens the configured root's worktree and common administration directories.
///
/// Returns `None` when the root has no `.git` entry. A present entry must be a
/// directory or a regular `gitdir:` marker; linked-worktree `commondir` markers
/// are resolved and the descriptor bindings are checked before returning.
pub fn open_repository_administration(
    root: &Path,
) -> Result<Option<RepositoryAdministrationDirectories>, crate::LocalGitToolsConstructionError> {
    use crate::LocalGitToolsConstructionError as Error;
    let root = File::from(
        rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Error::Repository)?,
    );
    match statat(&root, ".git", AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(Error::Repository),
        Ok(_) => {}
    }
    let directories = AdministrationDirectories::open(&root).map_err(|_| Error::Repository)?;
    validate_binding(&root, directories.binding).map_err(|_| Error::Repository)?;
    Ok(Some(RepositoryAdministrationDirectories {
        worktree: directories.worktree,
        common: directories.common,
    }))
}

pub(super) fn require_branch_unoccupied(
    authority: &crate::pinning::PinnedRepository,
    reference: &str,
) -> Result<(), LocalGitFailure> {
    let (requested_chain, _) =
        crate::reference_read::resolve_pinned_reference_chain_from(authority, reference, None)?;
    let requested = requested_chain.last().ok_or(LocalGitFailure::Operation)?;
    let current = file_identity(&authority.worktree_directory.metadata().map_err(rejected)?);
    let inspect = |directory: &File| -> Result<(), LocalGitFailure> {
        if file_identity(&directory.metadata().map_err(rejected)?) == current {
            return Ok(());
        }
        let (head, _) = read_marker(directory, "HEAD")?;
        let head = head.strip_suffix(b"\n").unwrap_or(&head);
        if let Some(target) = head.strip_prefix(b"ref: ") {
            let target = std::str::from_utf8(target).map_err(rejected)?;
            let (chain, _) = crate::reference_read::resolve_pinned_reference_chain_from(
                authority, target, None,
            )?;
            if chain.last() == Some(requested) {
                return Err(LocalGitFailure::Operation);
            }
        }
        Ok(())
    };
    inspect(&authority.git_directory)?;
    let worktrees = match openat(
        &authority.git_directory,
        "worktrees",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => File::from(directory),
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(rejected(error)),
    };
    let mut entries = rustix::fs::Dir::read_from(&worktrees).map_err(rejected)?;
    let mut inspected = 0;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(rejected)?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == OsStr::new(".") || name == OsStr::new("..") {
            continue;
        }
        inspected += 1;
        if inspected > crate::limits::MAX_REPOSITORY_INSPECTIONS {
            return Err(LocalGitFailure::Repository);
        }
        inspect(&open_directory(&worktrees, Path::new(name))?)?;
    }
    Ok(())
}
