use std::{
    ffi::OsStr,
    fs,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::PathBuf,
};

use rustix::fs::{AtFlags, statat};

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
    pub(super) config: FileIdentity,
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
