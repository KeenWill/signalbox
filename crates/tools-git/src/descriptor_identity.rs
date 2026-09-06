use std::{ffi::OsStr, fs, os::fd::BorrowedFd};

use rustix::{
    fs::{Mode, OFlags, openat},
    io::dup,
};

use crate::descriptor::{FileIdentity, file_identity};

pub(super) fn descriptor_identity(fd: BorrowedFd<'_>) -> Option<FileIdentity> {
    fs::File::from(dup(fd).ok()?)
        .metadata()
        .ok()
        .map(|metadata| file_identity(&metadata))
}

pub(super) fn entry_identity(parent: BorrowedFd<'_>, name: &OsStr) -> Option<FileIdentity> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()
    .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
    .map(|metadata| file_identity(&metadata))
}

pub(super) fn entry_is(parent: BorrowedFd<'_>, name: &OsStr, expected: FileIdentity) -> bool {
    entry_identity(parent, name) == Some(expected)
}

pub(super) fn descriptor_and_path_agree(
    held: &fs::File,
    parent: BorrowedFd<'_>,
    name: &OsStr,
    expected: FileIdentity,
) -> bool {
    let descriptor_identity = held
        .metadata()
        .map(|metadata| file_identity(&metadata))
        .ok();
    let path_identity = entry_identity(parent, name);
    descriptor_identity == Some(expected) && path_identity == Some(expected)
}

pub(super) fn bind_still_holds<E>(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    pinned: &fs::File,
    err: fn() -> E,
) -> Result<(), E> {
    let expected = file_identity(&pinned.metadata().map_err(|_| err())?);
    let current = fs::File::from(
        openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| err())?,
    );
    if file_identity(&current.metadata().map_err(|_| err())?) != expected {
        return Err(err());
    }
    Ok(())
}
