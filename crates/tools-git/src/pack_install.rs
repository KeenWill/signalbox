use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Seek},
    os::{
        fd::OwnedFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::Path,
};

use rustix::{
    fs::{AtFlags, Mode, OFlags, RenameFlags, openat, renameat_with, unlinkat},
    io::dup,
};

use crate::descriptor::{FileIdentity, file_identity};
use crate::failure::LocalGitFailure;
use crate::pinning::PinnedObjectDatabase;

pub(super) const OBJECT_PUBLICATION_LOCK: &str = "signalbox-publication.lock";

pub(super) struct ObjectPublicationLock {
    pub(super) directory: OwnedFd,
    descriptor: fs::File,
    identity: FileIdentity,
}

impl ObjectPublicationLock {
    pub(super) fn acquire(objects: &PinnedObjectDatabase) -> Result<Self, LocalGitFailure> {
        let directory = dup(objects.pack_directory()).map_err(|_| LocalGitFailure::Operation)?;
        let descriptor = openat(
            &directory,
            OBJECT_PUBLICATION_LOCK,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map(fs::File::from)
        .map_err(|_| LocalGitFailure::Operation)?;
        let identity = file_identity(
            &descriptor
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?,
        );
        Ok(Self {
            directory,
            descriptor,
            identity,
        })
    }
}

impl Drop for ObjectPublicationLock {
    fn drop(&mut self) {
        if pack_entry_is_owned(
            &self.directory,
            OsStr::new(OBJECT_PUBLICATION_LOCK),
            &self.descriptor,
            self.identity,
        ) {
            let _ = unlinkat(&self.directory, OBJECT_PUBLICATION_LOCK, AtFlags::empty());
        }
    }
}

pub(super) fn pack_installation_mode(pack_directory: &OwnedFd) -> Result<Mode, LocalGitFailure> {
    let metadata = fs::File::from(dup(pack_directory).map_err(|_| LocalGitFailure::Operation)?)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    let mode = (metadata.mode() & 0o666) | 0o600;
    Ok(Mode::from_raw_mode(mode))
}

#[derive(Debug)]
pub(super) struct InstalledPackedObject {
    descriptor: fs::File,
    identity: FileIdentity,
    name: OsString,
    created: bool,
}

impl InstalledPackedObject {
    fn rollback(&self, pack_directory: &OwnedFd) {
        if self.created
            && pack_entry_is_owned(pack_directory, &self.name, &self.descriptor, self.identity)
        {
            let _ = unlinkat(pack_directory, &self.name, AtFlags::empty());
        }
    }
}

pub(super) fn install_packed_object_pair(
    pack_directory: &OwnedFd,
    pack_source: &Path,
    index_source: &Path,
    mode: Mode,
) -> Result<(), LocalGitFailure> {
    let installed_pack = install_packed_object_file(pack_directory, pack_source, mode)?;
    match install_packed_object_file(pack_directory, index_source, mode) {
        Ok(_) => Ok(()),
        Err(failure) => {
            installed_pack.rollback(pack_directory);
            Err(failure)
        }
    }
}

pub(super) fn install_packed_object_file(
    pack_directory: &OwnedFd,
    source_path: &Path,
    mode: Mode,
) -> Result<InstalledPackedObject, LocalGitFailure> {
    install_packed_object_file_with_hook(pack_directory, source_path, mode, || {})
}

pub(super) fn install_packed_object_file_with_hook<Hook: FnOnce()>(
    pack_directory: &OwnedFd,
    source_path: &Path,
    mode: Mode,
    before_publish: Hook,
) -> Result<InstalledPackedObject, LocalGitFailure> {
    install_packed_object_file_with_copy_and_hook(
        pack_directory,
        source_path,
        mode,
        std::io::copy,
        before_publish,
    )
}

pub(super) fn install_packed_object_file_with_copy_and_hook<Copy, Hook>(
    pack_directory: &OwnedFd,
    source_path: &Path,
    mode: Mode,
    copy: Copy,
    before_publish: Hook,
) -> Result<InstalledPackedObject, LocalGitFailure>
where
    Copy: FnOnce(&mut fs::File, &mut fs::File) -> std::io::Result<u64>,
    Hook: FnOnce(),
{
    let name = source_path.file_name().ok_or(LocalGitFailure::Operation)?;
    let mut temporary_name = OsString::from(name);
    temporary_name.push(".lock");
    let mut source = fs::File::open(source_path).map_err(|_| LocalGitFailure::Operation)?;
    let source_length = source
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?
        .len();
    let descriptor = openat(
        pack_directory,
        &temporary_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        mode,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let mut destination = fs::File::from(descriptor);
    let identity = file_identity(
        &destination
            .metadata()
            .map_err(|_| LocalGitFailure::Operation)?,
    );
    if destination
        .set_permissions(fs::Permissions::from_mode(mode.bits()))
        .is_err()
    {
        remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
        return Err(LocalGitFailure::Operation);
    }
    let copied = match copy(&mut source, &mut destination) {
        Ok(copied) => copied,
        Err(_) => {
            remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
            return Err(LocalGitFailure::Operation);
        }
    };
    if copied != source_length {
        remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
        return Err(LocalGitFailure::Operation);
    }
    if destination.sync_all().is_err() {
        remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
        return Err(LocalGitFailure::Operation);
    }
    before_publish();
    if !pack_lock_is_owned(pack_directory, &temporary_name, &destination, identity) {
        return Err(LocalGitFailure::Operation);
    }
    match renameat_with(
        pack_directory,
        &temporary_name,
        pack_directory,
        name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(InstalledPackedObject {
            descriptor: destination,
            identity,
            name: name.to_owned(),
            created: true,
        }),
        Err(error) if error == rustix::io::Errno::EXIST => {
            remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
            let existing = openat(
                pack_directory,
                name,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            let mut existing = fs::File::from(existing);
            let metadata = existing
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?;
            if metadata.is_file()
                && metadata.len() == source_length
                && files_have_equal_content(&mut source, &mut existing)?
            {
                Ok(InstalledPackedObject {
                    descriptor: destination,
                    identity,
                    name: name.to_owned(),
                    created: false,
                })
            } else {
                Err(LocalGitFailure::Operation)
            }
        }
        Err(_) => {
            remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
            Err(LocalGitFailure::Operation)
        }
    }
}

pub(super) fn pack_lock_is_owned(
    pack_directory: &OwnedFd,
    temporary_name: &OsStr,
    destination: &fs::File,
    identity: FileIdentity,
) -> bool {
    pack_entry_is_owned(pack_directory, temporary_name, destination, identity)
}

pub(super) fn pack_entry_is_owned(
    pack_directory: &OwnedFd,
    name: &OsStr,
    descriptor: &fs::File,
    identity: FileIdentity,
) -> bool {
    let descriptor_identity = descriptor
        .metadata()
        .ok()
        .map(|metadata| file_identity(&metadata));
    let path_identity = openat(
        pack_directory,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()
    .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
    .map(|metadata| file_identity(&metadata));
    descriptor_identity == Some(identity) && path_identity == Some(identity)
}

pub(super) fn remove_owned_pack_lock(
    pack_directory: &OwnedFd,
    temporary_name: &OsStr,
    destination: &fs::File,
    identity: FileIdentity,
) {
    if pack_lock_is_owned(pack_directory, temporary_name, destination, identity) {
        let _ = unlinkat(pack_directory, temporary_name, AtFlags::empty());
    }
}

pub(super) fn files_have_equal_content(
    first: &mut fs::File,
    second: &mut fs::File,
) -> Result<bool, LocalGitFailure> {
    first
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|_| LocalGitFailure::Operation)?;
    second
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|_| LocalGitFailure::Operation)?;
    let mut first_buffer = [0_u8; 8192];
    let mut second_buffer = [0_u8; 8192];
    loop {
        let first_read = first
            .read(&mut first_buffer)
            .map_err(|_| LocalGitFailure::Operation)?;
        let second_read = second
            .read(&mut second_buffer)
            .map_err(|_| LocalGitFailure::Operation)?;
        if first_read != second_read || first_buffer[..first_read] != second_buffer[..second_read] {
            return Ok(false);
        }
        if first_read == 0 {
            return Ok(true);
        }
    }
}
