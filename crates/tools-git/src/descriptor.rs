use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::PathBuf,
};

use rustix::{
    fs::{AtFlags, FileType, Mode, OFlags, openat, statat, unlinkat},
    io::dup,
};

use crate::failure::LocalGitFailure;
use crate::limits::MAX_PACKED_REFS_BYTES;

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
}

fn clear_pinned_directory(directory: &OwnedFd) -> Result<(), LocalGitFailure> {
    let entries =
        fs::read_dir(descriptor_path_from_fd(directory)).map_err(|_| LocalGitFailure::Operation)?;
    for entry in entries {
        let name = entry.map_err(|_| LocalGitFailure::Operation)?.file_name();
        let status = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| LocalGitFailure::Operation)?;
        let identity = stat_file_identity(&status);
        let removal_flags = if FileType::from_raw_mode(status.st_mode) == FileType::Directory {
            let child = openat(
                directory,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            clear_pinned_directory(&child)?;
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        let current = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map(|status| stat_file_identity(&status))
            .map_err(|_| LocalGitFailure::Operation)?;
        if current != identity {
            return Err(LocalGitFailure::Operation);
        }
        unlinkat(directory, &name, removal_flags).map_err(|_| LocalGitFailure::Operation)?;
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

fn restore_or_remove_quarantined_entry(
    quarantine: &QuarantineDirectory,
    quarantined_name: &OsStr,
    parent: &OwnedFd,
    name: &OsStr,
    expected: FileIdentity,
    removal_flags: AtFlags,
) -> Result<(), LocalGitFailure> {
    if rustix::fs::renameat_with(
        quarantine.descriptor(),
        quarantined_name,
        parent,
        name,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .is_ok()
    {
        return Ok(());
    }
    let current = statat(
        quarantine.descriptor(),
        quarantined_name,
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .ok()
    .map(|status| stat_file_identity(&status));
    if current != Some(expected) {
        return Err(LocalGitFailure::Operation);
    }
    unlinkat(quarantine.descriptor(), quarantined_name, removal_flags)
        .map_err(|_| LocalGitFailure::Operation)
}

impl Drop for QuarantineDirectory {
    fn drop(&mut self) {
        let _ = clear_pinned_directory(&self.directory);
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
    let current = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(status) => Some(stat_file_identity(&status)),
        Err(error) if error == rustix::io::Errno::NOENT => None,
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    if current != Some(expected) {
        return Err(LocalGitFailure::Operation);
    }
    let quarantine = QuarantineDirectory::create(parent)?;
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
        restore_or_remove_quarantined_entry(
            &quarantine,
            quarantined_name,
            parent,
            name,
            expected,
            removal_flags,
        )?;
        return Err(LocalGitFailure::Operation);
    }
    if unlinkat(quarantine.descriptor(), quarantined_name, removal_flags).is_err() {
        restore_or_remove_quarantined_entry(
            &quarantine,
            quarantined_name,
            parent,
            name,
            expected,
            removal_flags,
        )?;
        return Err(LocalGitFailure::Operation);
    }
    if descriptor_entry_exists(parent, name)? {
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}
