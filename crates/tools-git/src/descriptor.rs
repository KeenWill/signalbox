use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
};

use rustix::{
    fs::{AtFlags, FileType, Mode, OFlags, openat, statat, unlinkat},
    io::dup,
};

use crate::failure::LocalGitFailure;
use crate::limits::{MAX_PACKED_REFS_BYTES, MAX_WORKTREE_INSPECTIONS};

pub(super) const MAX_QUARANTINE_DEPTH: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FileIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
}

pub(super) fn stat_file_identity(status: &rustix::fs::Stat) -> FileIdentity {
    // rustix exposes the host's native stat field widths; MetadataExt and the
    // repository identity contract use a stable u64 representation.
    #[allow(clippy::unnecessary_cast)]
    FileIdentity {
        device: status.st_dev as u64,
        inode: status.st_ino as u64,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RepositoryIdentity {
    pub(super) root: FileIdentity,
    pub(super) git_directory: FileIdentity,
    pub(super) refs: FileIdentity,
    pub(super) config: FileIdentity,
    pub(super) head: FileIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FileSnapshotIdentity {
    pub(super) file: FileIdentity,
    pub(super) length: u64,
    pub(super) modified_seconds: i64,
    pub(super) modified_nanoseconds: i64,
    pub(super) changed_seconds: i64,
    pub(super) changed_nanoseconds: i64,
}

pub(super) struct QuarantineDirectory {
    parent: OwnedFd,
    name: OsString,
    identity: FileIdentity,
    directory: OwnedFd,
    clear_on_drop: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct QuarantineSnapshot {
    entries: BTreeMap<PathBuf, QuarantineSnapshotEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuarantineSnapshotEntry {
    Directory {
        identity: FileIdentity,
        mode: u32,
    },
    Other {
        identity: FileSnapshotIdentity,
        mode: u32,
    },
}

impl QuarantineSnapshot {
    pub(super) fn without_subtree(&self, path: &Path) -> Self {
        Self {
            entries: self
                .entries
                .iter()
                .filter(|(entry, _)| entry.as_path() != path && !entry.starts_with(path))
                .map(|(path, entry)| (path.clone(), *entry))
                .collect(),
        }
    }
}

impl QuarantineDirectory {
    pub(super) fn create(parent: &OwnedFd) -> Result<Self, LocalGitFailure> {
        Self::create_with_hook(parent, || {})
    }

    fn create_with_hook<AfterPersist: FnOnce()>(
        parent: &OwnedFd,
        after_persist: AfterPersist,
    ) -> Result<Self, LocalGitFailure> {
        let pinned_parent = dup(parent).map_err(|_| LocalGitFailure::Operation)?;
        let temporary = tempfile::Builder::new()
            .prefix(".signalbox-cleanup-")
            .tempdir_in(descriptor_path_from_fd(parent))
            .map_err(|_| LocalGitFailure::Operation)?;
        let created_metadata =
            fs::symlink_metadata(temporary.path()).map_err(|_| LocalGitFailure::Operation)?;
        if created_metadata.file_type().is_symlink() || !created_metadata.is_dir() {
            return Err(LocalGitFailure::Operation);
        }
        let created_identity = file_identity(&created_metadata);
        let name = temporary
            .path()
            .file_name()
            .ok_or(LocalGitFailure::Operation)?
            .to_owned();
        let _persisted_path = temporary.keep();
        after_persist();
        let directory = match openat(
            parent,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(_) => {
                remove_quarantine_directory_if_identity(parent, &name, created_identity)?;
                return Err(LocalGitFailure::Operation);
            }
        };
        let identity = match dup(&directory)
            .ok()
            .map(fs::File::from)
            .and_then(|file| file.metadata().ok())
            .map(|metadata| file_identity(&metadata))
        {
            Some(identity) if identity == created_identity => identity,
            Some(_) | None => {
                drop(directory);
                remove_quarantine_directory_if_identity(parent, &name, created_identity)?;
                return Err(LocalGitFailure::Operation);
            }
        };
        Ok(Self {
            parent: pinned_parent,
            name,
            identity,
            directory,
            clear_on_drop: true,
        })
    }

    #[cfg(test)]
    pub(super) fn create_with_test_hook<AfterPersist: FnOnce()>(
        parent: &OwnedFd,
        after_persist: AfterPersist,
    ) -> Result<Self, LocalGitFailure> {
        Self::create_with_hook(parent, after_persist)
    }

    pub(super) fn descriptor(&self) -> &OwnedFd {
        &self.directory
    }

    pub(super) fn name(&self) -> &OsStr {
        &self.name
    }

    pub(super) fn keep(&mut self) {
        self.clear_on_drop = false;
    }

    pub(super) fn remove_if_empty_and_current(&self) -> Result<(), LocalGitFailure> {
        remove_quarantine_directory_if_identity(&self.parent, &self.name, self.identity)
    }

    pub(super) fn snapshot(&self) -> Result<QuarantineSnapshot, LocalGitFailure> {
        let mut entries = BTreeMap::new();
        let mut inspections = 0_usize;
        snapshot_pinned_directory_bounded(
            &self.directory,
            Path::new(""),
            0,
            &mut inspections,
            &mut entries,
        )?;
        Ok(QuarantineSnapshot { entries })
    }

    pub(super) fn clear_if_unchanged(
        &mut self,
        expected: &QuarantineSnapshot,
    ) -> Result<(), LocalGitFailure> {
        self.clear_if_unchanged_with_hook(expected, || {})
    }

    fn clear_if_unchanged_with_hook<AfterSnapshot: FnOnce()>(
        &mut self,
        expected: &QuarantineSnapshot,
        after_snapshot: AfterSnapshot,
    ) -> Result<(), LocalGitFailure> {
        self.keep();
        if self.snapshot().as_ref() != Ok(expected) {
            return Err(LocalGitFailure::Operation);
        }
        after_snapshot();
        let mut inspections = 0_usize;
        clear_snapshot_entries_bounded(
            &self.directory,
            Path::new(""),
            0,
            &mut inspections,
            expected,
        )?;
        remove_entry_if_identity(&self.parent, &self.name, self.identity, AtFlags::REMOVEDIR)
    }

    #[cfg(test)]
    pub(super) fn clear_if_unchanged_with_test_hook<AfterSnapshot: FnOnce()>(
        &mut self,
        expected: &QuarantineSnapshot,
        after_snapshot: AfterSnapshot,
    ) -> Result<(), LocalGitFailure> {
        self.clear_if_unchanged_with_hook(expected, after_snapshot)
    }
}

fn snapshot_pinned_directory_bounded(
    directory: &OwnedFd,
    prefix: &Path,
    depth: usize,
    inspections: &mut usize,
    entries: &mut BTreeMap<PathBuf, QuarantineSnapshotEntry>,
) -> Result<(), LocalGitFailure> {
    if depth > MAX_QUARANTINE_DEPTH {
        return Err(LocalGitFailure::Operation);
    }
    let children =
        fs::read_dir(descriptor_path_from_fd(directory)).map_err(|_| LocalGitFailure::Operation)?;
    for child in children {
        *inspections = inspections
            .checked_add(1)
            .filter(|count| *count <= MAX_WORKTREE_INSPECTIONS)
            .ok_or(LocalGitFailure::Operation)?;
        let name = child.map_err(|_| LocalGitFailure::Operation)?.file_name();
        let path = prefix.join(&name);
        let status = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| LocalGitFailure::Operation)?;
        #[allow(clippy::unnecessary_cast)]
        let mode = status.st_mode as u32;
        if FileType::from_raw_mode(status.st_mode) == FileType::Directory {
            entries.insert(
                path.clone(),
                QuarantineSnapshotEntry::Directory {
                    identity: stat_file_identity(&status),
                    mode,
                },
            );
            let child = openat(
                directory,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            snapshot_pinned_directory_bounded(&child, &path, depth + 1, inspections, entries)?;
        } else {
            #[allow(clippy::unnecessary_cast)]
            let identity = FileSnapshotIdentity {
                file: stat_file_identity(&status),
                length: status.st_size as u64,
                modified_seconds: status.st_mtime as i64,
                modified_nanoseconds: status.st_mtime_nsec as i64,
                changed_seconds: status.st_ctime as i64,
                changed_nanoseconds: status.st_ctime_nsec as i64,
            };
            entries.insert(path, QuarantineSnapshotEntry::Other { identity, mode });
        }
    }
    Ok(())
}

fn clear_snapshot_entries_bounded(
    directory: &OwnedFd,
    prefix: &Path,
    depth: usize,
    inspections: &mut usize,
    expected: &QuarantineSnapshot,
) -> Result<(), LocalGitFailure> {
    if depth > MAX_QUARANTINE_DEPTH {
        return Err(LocalGitFailure::Operation);
    }
    let entries = expected
        .entries
        .iter()
        .filter(|(path, _)| path.parent() == Some(prefix))
        .map(|(path, entry)| {
            path.file_name()
                .map(|name| (name.to_owned(), path.clone(), *entry))
                .ok_or(LocalGitFailure::Operation)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for (name, path, expected_entry) in entries {
        *inspections = inspections
            .checked_add(1)
            .filter(|count| *count <= MAX_WORKTREE_INSPECTIONS)
            .ok_or(LocalGitFailure::Operation)?;
        let status = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| LocalGitFailure::Operation)?;
        #[allow(clippy::unnecessary_cast)]
        let mode = status.st_mode as u32;
        match expected_entry {
            QuarantineSnapshotEntry::Directory {
                identity,
                mode: expected_mode,
            } if FileType::from_raw_mode(status.st_mode) == FileType::Directory
                && stat_file_identity(&status) == identity
                && mode == expected_mode =>
            {
                let child = openat(
                    directory,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| LocalGitFailure::Operation)?;
                let metadata = fs::File::from(dup(&child).map_err(|_| LocalGitFailure::Operation)?)
                    .metadata()
                    .map_err(|_| LocalGitFailure::Operation)?;
                if file_identity(&metadata) != identity || metadata.mode() != expected_mode {
                    return Err(LocalGitFailure::Operation);
                }
                clear_snapshot_entries_bounded(&child, &path, depth + 1, inspections, expected)?;
                remove_entry_if_identity(directory, &name, identity, AtFlags::REMOVEDIR)?;
            }
            QuarantineSnapshotEntry::Other {
                identity,
                mode: expected_mode,
            } => {
                #[allow(clippy::unnecessary_cast)]
                let current = FileSnapshotIdentity {
                    file: stat_file_identity(&status),
                    length: status.st_size as u64,
                    modified_seconds: status.st_mtime as i64,
                    modified_nanoseconds: status.st_mtime_nsec as i64,
                    changed_seconds: status.st_ctime as i64,
                    changed_nanoseconds: status.st_ctime_nsec as i64,
                };
                if current != identity || mode != expected_mode {
                    return Err(LocalGitFailure::Operation);
                }
                remove_entry_if_identity(directory, &name, identity.file, AtFlags::empty())?;
            }
            QuarantineSnapshotEntry::Directory { .. } => {
                return Err(LocalGitFailure::Operation);
            }
        }
    }
    Ok(())
}

fn remove_quarantine_directory_if_identity(
    parent: &OwnedFd,
    name: &OsStr,
    expected: FileIdentity,
) -> Result<(), LocalGitFailure> {
    let current = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(status) => Some(stat_file_identity(&status)),
        Err(error) if error == rustix::io::Errno::NOENT => None,
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    match current {
        None => Ok(()),
        Some(identity) if identity == expected => {
            unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(|_| LocalGitFailure::Operation)
        }
        Some(_) => Err(LocalGitFailure::Operation),
    }
}

impl Drop for QuarantineDirectory {
    fn drop(&mut self) {
        if !self.clear_on_drop {
            return;
        }
        let current = openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
        .and_then(|directory| fs::File::from(directory).metadata().ok())
        .map(|metadata| file_identity(&metadata));
        if current == Some(self.identity) {
            let _ = unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR);
        }
    }
}

pub(super) fn descriptor_path_from_fd(file: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
}

pub(super) fn descriptor_path(file: &fs::File) -> PathBuf {
    PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
}

pub(super) fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

pub(super) fn file_snapshot_identity(metadata: &fs::Metadata) -> FileSnapshotIdentity {
    FileSnapshotIdentity {
        file: file_identity(metadata),
        length: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

pub(super) fn descriptor_entry_exists(
    parent: &OwnedFd,
    leaf: &OsStr,
) -> Result<bool, LocalGitFailure> {
    match statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(false),
        Err(_) => Err(LocalGitFailure::Operation),
    }
}

pub(super) fn unsupported_control_files_are_absent(
    git_directory: BorrowedFd<'_>,
) -> Result<(), LocalGitFailure> {
    unsupported_common_directory_is_absent(git_directory)?;
    unsupported_object_alternates_are_absent(git_directory)?;
    unsupported_packed_replacement_objects_are_absent(git_directory)?;
    unsupported_replacement_objects_are_absent(git_directory)
}

pub(super) fn unsupported_common_directory_is_absent(
    git_directory: BorrowedFd<'_>,
) -> Result<(), LocalGitFailure> {
    require_entry_absent(git_directory, OsStr::new("commondir"))
}

pub(super) fn unsupported_object_alternates_are_absent(
    git_directory: BorrowedFd<'_>,
) -> Result<(), LocalGitFailure> {
    unsupported_object_alternates_are_absent_with_hook(git_directory, || {})
}

#[cfg(test)]
pub(super) fn unsupported_object_alternates_are_absent_with_test_hook<Hook: FnOnce()>(
    git_directory: BorrowedFd<'_>,
    after_absence_check: Hook,
) -> Result<(), LocalGitFailure> {
    unsupported_object_alternates_are_absent_with_hook(git_directory, after_absence_check)
}

fn unsupported_object_alternates_are_absent_with_hook<Hook: FnOnce()>(
    git_directory: BorrowedFd<'_>,
    after_absence_check: Hook,
) -> Result<(), LocalGitFailure> {
    let mut after_absence_check = Some(after_absence_check);
    let objects = openat(
        git_directory,
        "objects",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    let objects_identity = owned_descriptor_identity(&objects)?;
    let info = match openat(
        &objects,
        "info",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(info) => info,
        Err(error) if error == rustix::io::Errno::NOENT => {
            after_absence_check
                .take()
                .ok_or(LocalGitFailure::Repository)?();
            let current_objects =
                reopen_bound_directory(git_directory, "objects", objects_identity)?;
            match openat(
                &current_objects,
                "info",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Err(error) if error == rustix::io::Errno::NOENT => return Ok(()),
                Ok(_) | Err(_) => return Err(LocalGitFailure::Repository),
            }
        }
        Err(_) => return Err(LocalGitFailure::Repository),
    };
    let info_identity = owned_descriptor_identity(&info)?;
    require_entry_absent(info.as_fd(), OsStr::new("alternates"))?;
    after_absence_check
        .take()
        .ok_or(LocalGitFailure::Repository)?();
    let current_objects = reopen_bound_directory(git_directory, "objects", objects_identity)?;
    let current_info = reopen_bound_directory(current_objects.as_fd(), "info", info_identity)?;
    require_entry_absent(current_info.as_fd(), OsStr::new("alternates"))
}

fn unsupported_replacement_objects_are_absent(
    git_directory: BorrowedFd<'_>,
) -> Result<(), LocalGitFailure> {
    let info = match openat(
        git_directory,
        "info",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(info) => Some(info),
        Err(error) if error == rustix::io::Errno::NOENT => None,
        Err(_) => return Err(LocalGitFailure::Repository),
    };
    match info {
        Some(info) => {
            let identity = owned_descriptor_identity(&info)?;
            require_entry_absent(info.as_fd(), OsStr::new("grafts"))?;
            let current = reopen_bound_directory(git_directory, "info", identity)?;
            require_entry_absent(current.as_fd(), OsStr::new("grafts"))?;
        }
        None => match openat(
            git_directory,
            "info",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Err(error) if error == rustix::io::Errno::NOENT => {}
            Ok(_) | Err(_) => return Err(LocalGitFailure::Repository),
        },
    }
    let refs = openat(
        git_directory,
        "refs",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    let refs_identity = owned_descriptor_identity(&refs)?;
    require_entry_absent(refs.as_fd(), OsStr::new("replace"))?;
    let current_refs = reopen_bound_directory(git_directory, "refs", refs_identity)?;
    require_entry_absent(current_refs.as_fd(), OsStr::new("replace"))
}

fn unsupported_packed_replacement_objects_are_absent(
    git_directory: BorrowedFd<'_>,
) -> Result<(), LocalGitFailure> {
    let descriptor = match openat(
        git_directory,
        "packed-refs",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => {
            return match openat(
                git_directory,
                "packed-refs",
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Err(error) if error == rustix::io::Errno::NOENT => Ok(()),
                Ok(_) | Err(_) => Err(LocalGitFailure::Repository),
            };
        }
        Err(_) => return Err(LocalGitFailure::Repository),
    };
    let mut file = fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
    if !metadata.is_file() || metadata.len() > MAX_PACKED_REFS_BYTES as u64 {
        return Err(LocalGitFailure::Repository);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_PACKED_REFS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Repository)?;
    let after_read = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
    if bytes.len() > MAX_PACKED_REFS_BYTES
        || bytes.len() as u64 != metadata.len()
        || file_snapshot_identity(&metadata) != file_snapshot_identity(&after_read)
        || bytes
            .windows(b" refs/replace/".len())
            .any(|window| window == b" refs/replace/")
    {
        return Err(LocalGitFailure::Repository);
    }
    let current = openat(
        git_directory,
        "packed-refs",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    let current_metadata = fs::File::from(current)
        .metadata()
        .map_err(|_| LocalGitFailure::Repository)?;
    if file_snapshot_identity(&current_metadata) != file_snapshot_identity(&after_read) {
        return Err(LocalGitFailure::Repository);
    }
    Ok(())
}

fn owned_descriptor_identity(directory: &OwnedFd) -> Result<FileIdentity, LocalGitFailure> {
    fs::File::from(dup(directory).map_err(|_| LocalGitFailure::Repository)?)
        .metadata()
        .map(|metadata| file_identity(&metadata))
        .map_err(|_| LocalGitFailure::Repository)
}

fn reopen_bound_directory(
    parent: BorrowedFd<'_>,
    name: &str,
    expected: FileIdentity,
) -> Result<OwnedFd, LocalGitFailure> {
    let current = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    if owned_descriptor_identity(&current)? != expected {
        return Err(LocalGitFailure::Repository);
    }
    Ok(current)
}

fn require_entry_absent(parent: BorrowedFd<'_>, name: &OsStr) -> Result<(), LocalGitFailure> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(error) if error == rustix::io::Errno::NOENT => Ok(()),
        _ => Err(LocalGitFailure::Repository),
    }
}

pub(super) fn remove_entry_if_identity(
    parent: &OwnedFd,
    name: &OsStr,
    expected: FileIdentity,
    removal_flags: AtFlags,
) -> Result<(), LocalGitFailure> {
    remove_entry_if_identity_with_hook(parent, name, expected, removal_flags, |_| {})
}

fn remove_entry_if_identity_with_hook<AfterQuarantine: FnOnce(&QuarantineDirectory)>(
    parent: &OwnedFd,
    name: &OsStr,
    expected: FileIdentity,
    removal_flags: AtFlags,
    after_quarantine: AfterQuarantine,
) -> Result<(), LocalGitFailure> {
    let current = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(status) => Some(stat_file_identity(&status)),
        Err(error) if error == rustix::io::Errno::NOENT => None,
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    if current != Some(expected) {
        return Err(LocalGitFailure::Operation);
    }
    let mut quarantine = QuarantineDirectory::create(parent)?;
    quarantine.keep();
    after_quarantine(&quarantine);
    let quarantined_name = OsStr::new("owned");
    rustix::fs::renameat_with(
        parent,
        name,
        quarantine.descriptor(),
        quarantined_name,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let current = statat(
        quarantine.descriptor(),
        quarantined_name,
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .ok()
    .map(|status| stat_file_identity(&status));
    if current != Some(expected) {
        let restoration = rustix::fs::renameat_with(
            quarantine.descriptor(),
            quarantined_name,
            parent,
            name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|_| LocalGitFailure::Operation);
        let cleanup = restoration.and_then(|()| quarantine.remove_if_empty_and_current());
        return Err(cleanup.err().unwrap_or(LocalGitFailure::Operation));
    }
    if unlinkat(quarantine.descriptor(), quarantined_name, removal_flags).is_err() {
        let restoration = rustix::fs::renameat_with(
            quarantine.descriptor(),
            quarantined_name,
            parent,
            name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|_| LocalGitFailure::Operation);
        let cleanup = restoration.and_then(|()| quarantine.remove_if_empty_and_current());
        return Err(cleanup.err().unwrap_or(LocalGitFailure::Operation));
    }
    quarantine.remove_if_empty_and_current()?;
    if descriptor_entry_exists(parent, name)? {
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn remove_entry_if_identity_with_test_hook<
    AfterQuarantine: FnOnce(&QuarantineDirectory),
>(
    parent: &OwnedFd,
    name: &OsStr,
    expected: FileIdentity,
    removal_flags: AtFlags,
    after_quarantine: AfterQuarantine,
) -> Result<(), LocalGitFailure> {
    remove_entry_if_identity_with_hook(parent, name, expected, removal_flags, after_quarantine)
}
