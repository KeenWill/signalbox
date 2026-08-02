use std::{
    ffi::{OsStr, OsString},
    fs,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::PathBuf,
};

use rustix::{
    fs::{AtFlags, Mode, OFlags, openat, statat, unlinkat},
    io::dup,
};

use crate::failure::LocalGitFailure;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FileIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
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
}

fn remove_quarantine_directory_if_identity(
    parent: &OwnedFd,
    name: &OsStr,
    expected: FileIdentity,
) -> Result<(), LocalGitFailure> {
    let current = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(status) => Some(FileIdentity {
            device: status.st_dev,
            inode: status.st_ino,
        }),
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
    .map(|status| FileIdentity {
        device: status.st_dev,
        inode: status.st_ino,
    });
    if current != Some(expected) {
        return Err(LocalGitFailure::Operation);
    }
    unlinkat(quarantine.descriptor(), quarantined_name, removal_flags)
        .map_err(|_| LocalGitFailure::Operation)
}

impl Drop for QuarantineDirectory {
    fn drop(&mut self) {
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
    unsupported_object_alternates_are_absent(git_directory)
}

pub(super) fn unsupported_common_directory_is_absent(
    git_directory: BorrowedFd<'_>,
) -> Result<(), LocalGitFailure> {
    require_entry_absent(git_directory, OsStr::new("commondir"))
}

pub(super) fn unsupported_object_alternates_are_absent(
    git_directory: BorrowedFd<'_>,
) -> Result<(), LocalGitFailure> {
    let objects = openat(
        git_directory,
        "objects",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    let info = match openat(
        &objects,
        "info",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(info) => info,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(()),
        Err(_) => return Err(LocalGitFailure::Repository),
    };
    require_entry_absent(info.as_fd(), OsStr::new("alternates"))
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
    .map(|status| FileIdentity {
        device: status.st_dev,
        inode: status.st_ino,
    });
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
