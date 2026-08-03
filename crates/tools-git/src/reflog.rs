use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Seek, Write},
    os::{
        fd::OwnedFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path},
};

use git2::Signature;
use rustix::{
    fs::{AtFlags, Mode, OFlags, RenameFlags, openat, renameat_with, unlinkat},
    io::dup,
};
use sha2::{Digest, Sha256};

use crate::descriptor::{FileIdentity, file_identity};
use crate::failure::LocalGitFailure;
use crate::limits::MAX_REFLOG_BYTES;
use crate::pack_install::{pack_lock_is_owned, remove_owned_pack_lock};
use crate::pinning::PinnedRepository;
use crate::reference_lock::{CreatedReferenceDirectories, reference_installation_modes};

pub(super) struct ReferenceLogLock {
    parent: OwnedFd,
    leaf: OsString,
    lock_name: OsString,
    lock: fs::File,
    identity: FileIdentity,
    backup: Option<fs::File>,
    original_permissions: Option<fs::Permissions>,
    original_snapshot: Option<ReferenceLogSnapshot>,
    created_directories: CreatedReferenceDirectories,
    committed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReferenceLogSnapshot {
    identity: FileIdentity,
    length: u64,
    digest: [u8; 32],
}

impl ReferenceLogLock {
    pub(super) fn acquire(
        authority: &PinnedRepository,
        reference: &str,
    ) -> Result<Self, LocalGitFailure> {
        let git_directory =
            dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
        let refs = openat(
            &git_directory,
            "refs",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let installation_modes = reference_installation_modes(&refs)?;
        let directory_mode = installation_modes.directory;
        let file_mode = installation_modes.file;
        let mut created_directories = CreatedReferenceDirectories::default();
        let logs = created_directories.open_or_create(
            &git_directory,
            OsStr::new("logs"),
            directory_mode,
        )?;
        let path = Path::new(reference);
        let leaf = path
            .file_name()
            .filter(|leaf| !leaf.is_empty())
            .ok_or(LocalGitFailure::Operation)?
            .to_owned();
        let mut parent = logs;
        if let Some(components) = path.parent() {
            for component in components.components() {
                let Component::Normal(component) = component else {
                    return Err(LocalGitFailure::Operation);
                };
                parent = created_directories.open_or_create(&parent, component, directory_mode)?;
            }
        }
        let mut lock_name = OsString::from(&leaf);
        lock_name.push(".lock");
        let descriptor = openat(
            &parent,
            &lock_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            file_mode,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let lock = fs::File::from(descriptor);
        let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        let mut guard = Self {
            parent,
            leaf,
            lock_name,
            lock,
            identity,
            backup: None,
            original_permissions: None,
            original_snapshot: None,
            created_directories,
            committed: false,
        };
        guard
            .lock
            .set_permissions(fs::Permissions::from_mode(file_mode.bits()))
            .map_err(|_| LocalGitFailure::Operation)?;
        guard.copy_existing()?;
        Ok(guard)
    }

    fn copy_existing(&mut self) -> Result<(), LocalGitFailure> {
        let descriptor = match openat(
            &self.parent,
            &self.leaf,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(error) if error == rustix::io::Errno::NOENT => return Ok(()),
            Err(_) => return Err(LocalGitFailure::Operation),
        };
        let mut source = fs::File::from(descriptor);
        let metadata = source.metadata().map_err(|_| LocalGitFailure::Operation)?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_REFLOG_BYTES as u64
        {
            return Err(LocalGitFailure::Operation);
        }
        let permissions = fs::Permissions::from_mode(metadata.mode() & 0o777);
        self.lock
            .set_permissions(permissions.clone())
            .map_err(|_| LocalGitFailure::Operation)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut source)
            .take((MAX_REFLOG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| LocalGitFailure::Operation)?;
        let after_read = source.metadata().map_err(|_| LocalGitFailure::Operation)?;
        if bytes.len() as u64 != metadata.len()
            || bytes.len() > MAX_REFLOG_BYTES
            || file_identity(&after_read) != file_identity(&metadata)
            || after_read.len() != metadata.len()
        {
            return Err(LocalGitFailure::Operation);
        }
        self.lock
            .write_all(&bytes)
            .map_err(|_| LocalGitFailure::Operation)?;
        let mut backup = tempfile::tempfile().map_err(|_| LocalGitFailure::Operation)?;
        backup
            .write_all(&bytes)
            .map_err(|_| LocalGitFailure::Operation)?;
        backup.rewind().map_err(|_| LocalGitFailure::Operation)?;
        self.backup = Some(backup);
        self.original_permissions = Some(permissions);
        self.original_snapshot = Some(ReferenceLogSnapshot {
            identity: file_identity(&metadata),
            length: metadata.len(),
            digest: Sha256::digest(&bytes).into(),
        });
        Ok(())
    }

    pub(super) fn append(
        &mut self,
        old: git2::Oid,
        new: git2::Oid,
        signature: &Signature<'_>,
        action: &str,
    ) -> Result<(), LocalGitFailure> {
        let time = signature.when();
        let offset = time.offset_minutes();
        let sign = if offset < 0 { '-' } else { '+' };
        let absolute_offset = offset.unsigned_abs();
        writeln!(
            self.lock,
            "{old} {new} {} <{}> {} {sign}{:02}{:02}\t{action}",
            signature.name().map_err(|_| LocalGitFailure::Operation)?,
            signature.email().map_err(|_| LocalGitFailure::Operation)?,
            time.seconds(),
            absolute_offset / 60,
            absolute_offset % 60,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        if self
            .lock
            .metadata()
            .map_err(|_| LocalGitFailure::Operation)?
            .len()
            > MAX_REFLOG_BYTES as u64
        {
            return Err(LocalGitFailure::Operation);
        }
        self.lock.sync_all().map_err(|_| LocalGitFailure::Operation)
    }

    pub(super) fn publish(&mut self) -> Result<(), LocalGitFailure> {
        if !self.path_still_owned() || !self.original_still_matches()? {
            return Err(LocalGitFailure::Operation);
        }
        renameat_with(
            &self.parent,
            &self.lock_name,
            &self.parent,
            &self.leaf,
            RenameFlags::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        self.committed = true;
        self.created_directories.keep();
        Ok(())
    }

    pub(super) fn rollback(&mut self) -> Result<(), LocalGitFailure> {
        if !self.committed {
            return Ok(());
        }
        if let Some(backup) = &mut self.backup {
            let descriptor = openat(
                &self.parent,
                &self.lock_name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            let mut restoration = fs::File::from(descriptor);
            let identity = file_identity(
                &restoration
                    .metadata()
                    .map_err(|_| LocalGitFailure::Operation)?,
            );
            if let Some(permissions) = &self.original_permissions
                && restoration.set_permissions(permissions.clone()).is_err()
            {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
            if backup.rewind().is_err() {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
            let copied = match std::io::copy(
                &mut Read::by_ref(backup).take((MAX_REFLOG_BYTES + 1) as u64),
                &mut restoration,
            ) {
                Ok(copied) => copied,
                Err(_) => {
                    remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                    return Err(LocalGitFailure::Operation);
                }
            };
            if copied > MAX_REFLOG_BYTES as u64 {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
            if restoration.sync_all().is_err() {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
            if !pack_lock_is_owned(&self.parent, &self.lock_name, &restoration, identity) {
                return Err(LocalGitFailure::Operation);
            }
            let published_identity = openat(
                &self.parent,
                &self.leaf,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .ok()
            .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
            .map(|metadata| file_identity(&metadata));
            if published_identity != Some(self.identity) {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
            if renameat_with(
                &self.parent,
                &self.lock_name,
                &self.parent,
                &self.leaf,
                RenameFlags::empty(),
            )
            .is_err()
            {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
        } else {
            let published_identity = openat(
                &self.parent,
                &self.leaf,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .ok()
            .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
            .map(|metadata| file_identity(&metadata));
            if published_identity != Some(self.identity) {
                return Err(LocalGitFailure::Operation);
            }
            unlinkat(&self.parent, &self.leaf, AtFlags::empty())
                .map_err(|_| LocalGitFailure::Operation)?;
        }
        self.committed = false;
        Ok(())
    }

    fn path_still_owned(&self) -> bool {
        let descriptor_identity = self
            .lock
            .metadata()
            .map(|metadata| file_identity(&metadata))
            .ok();
        let path_identity = openat(
            &self.parent,
            &self.lock_name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
        .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
        .map(|metadata| file_identity(&metadata));
        descriptor_identity == Some(self.identity) && path_identity == Some(self.identity)
    }

    fn original_still_matches(&self) -> Result<bool, LocalGitFailure> {
        let descriptor = match openat(
            &self.parent,
            &self.leaf,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(error) if error == rustix::io::Errno::NOENT => {
                return Ok(self.original_snapshot.is_none());
            }
            Err(_) => return Err(LocalGitFailure::Operation),
        };
        let mut source = fs::File::from(descriptor);
        let metadata = source.metadata().map_err(|_| LocalGitFailure::Operation)?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_REFLOG_BYTES as u64
        {
            return Ok(false);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut source)
            .take((MAX_REFLOG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| LocalGitFailure::Operation)?;
        let after_read = source.metadata().map_err(|_| LocalGitFailure::Operation)?;
        let observed = ReferenceLogSnapshot {
            identity: file_identity(&metadata),
            length: metadata.len(),
            digest: Sha256::digest(&bytes).into(),
        };
        Ok(bytes.len() as u64 == metadata.len()
            && bytes.len() <= MAX_REFLOG_BYTES
            && file_identity(&after_read) == file_identity(&metadata)
            && after_read.len() == metadata.len()
            && self.original_snapshot == Some(observed))
    }
}

impl Drop for ReferenceLogLock {
    fn drop(&mut self) {
        if !self.committed && self.path_still_owned() {
            let _ = unlinkat(&self.parent, &self.lock_name, AtFlags::empty());
        }
    }
}
