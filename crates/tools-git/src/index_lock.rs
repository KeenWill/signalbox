use std::{
    fs,
    io::{Read, Seek, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use git2::Index;
use rustix::{
    fs::{CWD, Mode, OFlags, openat},
    io::dup,
};
use sha1::{Digest, Sha1};

use crate::descriptor::{FileIdentity, descriptor_path, file_identity};
use crate::failure::LocalGitFailure;
use crate::limits::MAX_INDEX_BYTES;
use crate::pinning::PinnedRepository;

pub(super) struct IndexLock {
    index_path: PathBuf,
    lock_path: PathBuf,
    lock: fs::File,
    identity: FileIdentity,
    _private_directory: tempfile::TempDir,
    private_index_path: PathBuf,
    committed: bool,
}

pub(super) struct IndexSnapshot {
    _file: fs::File,
}

impl IndexSnapshot {
    pub(super) fn acquire(index_path: &Path) -> Result<(Self, Index), LocalGitFailure> {
        let mut file = tempfile::tempfile().map_err(|_| LocalGitFailure::Operation)?;
        copy_index_snapshot(index_path, &mut file, false)?;
        let index = Index::open(&descriptor_path(&file)).map_err(|_| LocalGitFailure::Operation)?;
        Ok((Self { _file: file }, index))
    }
}

impl IndexLock {
    pub(super) fn acquire_for_repository(
        authority: &PinnedRepository,
    ) -> Result<(Self, Index), LocalGitFailure> {
        Self::acquire_with_private_directory_and_mode(
            &authority.git_path("index"),
            &authority.git_path("index.lock"),
            index_installation_mode(authority)?,
            tempfile::tempdir,
        )
    }

    #[cfg(test)]
    pub(super) fn acquire(
        index_path: &Path,
        lock_path: &Path,
    ) -> Result<(Self, Index), LocalGitFailure> {
        Self::acquire_with_private_directory_and_mode(
            index_path,
            lock_path,
            Mode::RUSR | Mode::WUSR,
            tempfile::tempdir,
        )
    }

    #[cfg(test)]
    pub(super) fn acquire_with_private_directory<Create>(
        index_path: &Path,
        lock_path: &Path,
        create_private_directory: Create,
    ) -> Result<(Self, Index), LocalGitFailure>
    where
        Create: FnOnce() -> std::io::Result<tempfile::TempDir>,
    {
        Self::acquire_with_private_directory_and_mode(
            index_path,
            lock_path,
            Mode::RUSR | Mode::WUSR,
            create_private_directory,
        )
    }

    fn acquire_with_private_directory_and_mode<Create>(
        index_path: &Path,
        lock_path: &Path,
        missing_index_mode: Mode,
        create_private_directory: Create,
    ) -> Result<(Self, Index), LocalGitFailure>
    where
        Create: FnOnce() -> std::io::Result<tempfile::TempDir>,
    {
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(lock_path)
            .map_err(|_| LocalGitFailure::Operation)?;
        let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        let private_directory = match create_private_directory() {
            Ok(directory) => directory,
            Err(_) => {
                remove_owned_index_lock(lock_path, &lock, identity);
                return Err(LocalGitFailure::Operation);
            }
        };
        let private_index_path = private_directory.path().join("index");
        let mut guard = Self {
            index_path: index_path.to_owned(),
            lock_path: lock_path.to_owned(),
            lock,
            identity,
            _private_directory: private_directory,
            private_index_path,
            committed: false,
        };
        guard
            .lock
            .set_permissions(fs::Permissions::from_mode(missing_index_mode.bits()))
            .map_err(|_| LocalGitFailure::Operation)?;
        copy_index_snapshot(index_path, &mut guard.lock, true)?;
        guard
            .lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        guard.copy_lock_to_private_index()?;
        let index =
            Index::open(&guard.private_index_path).map_err(|_| LocalGitFailure::Operation)?;
        Ok((guard, index))
    }

    fn copy_lock_to_private_index(&mut self) -> Result<(), LocalGitFailure> {
        let mut private_index = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&self.private_index_path)
            .map_err(|_| LocalGitFailure::Operation)?;
        self.lock.rewind().map_err(|_| LocalGitFailure::Operation)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut self.lock).take((MAX_INDEX_BYTES + 1) as u64),
            &mut private_index,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        if copied > MAX_INDEX_BYTES as u64 {
            return Err(LocalGitFailure::Operation);
        }
        private_index
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)
    }

    pub(super) fn original_bytes(&self) -> Result<Vec<u8>, LocalGitFailure> {
        let descriptor = openat(
            CWD,
            &self.private_index_path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let mut source = fs::File::from(descriptor);
        let metadata = source.metadata().map_err(|_| LocalGitFailure::Operation)?;
        if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES as u64 {
            return Err(LocalGitFailure::Operation);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut source)
            .take((MAX_INDEX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| LocalGitFailure::Operation)?;
        if bytes.len() as u64 != metadata.len() || bytes.len() > MAX_INDEX_BYTES {
            return Err(LocalGitFailure::Operation);
        }
        Ok(bytes)
    }

    pub(super) fn write_raw(&mut self, bytes: &[u8]) -> Result<(), LocalGitFailure> {
        if bytes.len() > MAX_INDEX_BYTES {
            return Err(LocalGitFailure::Operation);
        }
        self.lock
            .set_len(0)
            .and_then(|()| self.lock.rewind())
            .and_then(|()| self.lock.write_all(bytes))
            .and_then(|()| self.lock.sync_all())
            .map_err(|_| LocalGitFailure::Operation)
    }

    pub(super) fn write(&mut self, index: &mut Index) -> Result<(), LocalGitFailure> {
        if index.path() != Some(self.private_index_path.as_path()) {
            return write_index_entries(&mut self.lock, index);
        }
        index.write().map_err(|_| LocalGitFailure::Operation)?;
        let descriptor = openat(
            CWD,
            &self.private_index_path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let mut source = fs::File::from(descriptor);
        let metadata = source.metadata().map_err(|_| LocalGitFailure::Operation)?;
        if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES as u64 {
            return Err(LocalGitFailure::Operation);
        }
        self.lock
            .set_len(0)
            .map_err(|_| LocalGitFailure::Operation)?;
        self.lock.rewind().map_err(|_| LocalGitFailure::Operation)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut source).take((MAX_INDEX_BYTES + 1) as u64),
            &mut self.lock,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        if copied != metadata.len() || copied > MAX_INDEX_BYTES as u64 {
            return Err(LocalGitFailure::Operation);
        }
        self.lock.sync_all().map_err(|_| LocalGitFailure::Operation)
    }

    pub(super) fn commit(mut self) -> Result<FileIdentity, LocalGitFailure> {
        let path_identity = fs::symlink_metadata(&self.lock_path)
            .map(|metadata| file_identity(&metadata))
            .map_err(|_| LocalGitFailure::Operation)?;
        let descriptor_identity = file_identity(
            &self
                .lock
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?,
        );
        if path_identity != self.identity || descriptor_identity != self.identity {
            return Err(LocalGitFailure::Operation);
        }
        fs::rename(&self.lock_path, &self.index_path).map_err(|_| LocalGitFailure::Operation)?;
        self.committed = true;
        Ok(self.identity)
    }
}

pub(super) fn index_installation_mode(
    authority: &PinnedRepository,
) -> Result<Mode, LocalGitFailure> {
    let metadata =
        fs::File::from(dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?)
            .metadata()
            .map_err(|_| LocalGitFailure::Operation)?;
    Ok(Mode::from_raw_mode((metadata.mode() & 0o666) | 0o600))
}

pub(super) fn write_index_entries(
    destination: &mut fs::File,
    index: &Index,
) -> Result<(), LocalGitFailure> {
    let mut bytes = Vec::new();
    let version = if index.iter().any(|entry| entry.flags_extended != 0) {
        3_u32
    } else {
        2_u32
    };
    bytes.extend_from_slice(b"DIRC");
    bytes.extend_from_slice(&version.to_be_bytes());
    bytes.extend_from_slice(
        &u32::try_from(index.len())
            .map_err(|_| LocalGitFailure::Operation)?
            .to_be_bytes(),
    );
    for entry in index.iter() {
        let entry_start = bytes.len();
        for value in [
            entry.ctime.seconds() as u32,
            entry.ctime.nanoseconds(),
            entry.mtime.seconds() as u32,
            entry.mtime.nanoseconds(),
            entry.dev,
            entry.ino,
            entry.mode,
            entry.uid,
            entry.gid,
            entry.file_size,
        ] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(entry.id.as_bytes());
        let path_length = u16::try_from(entry.path.len())
            .unwrap_or(u16::MAX)
            .min(0x0fff);
        let extended = entry.flags_extended != 0;
        let flags = (entry.flags & !0x4fff) | path_length | if extended { 0x4000 } else { 0 };
        bytes.extend_from_slice(&flags.to_be_bytes());
        if extended {
            bytes.extend_from_slice(&entry.flags_extended.to_be_bytes());
        }
        bytes.extend_from_slice(&entry.path);
        bytes.push(0);
        while (bytes.len() - entry_start) % 8 != 0 {
            bytes.push(0);
        }
    }
    bytes.extend_from_slice(&Sha1::digest(&bytes));
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(LocalGitFailure::Operation);
    }
    destination
        .set_len(0)
        .map_err(|_| LocalGitFailure::Operation)?;
    destination
        .rewind()
        .map_err(|_| LocalGitFailure::Operation)?;
    destination
        .write_all(&bytes)
        .and_then(|()| destination.sync_all())
        .map_err(|_| LocalGitFailure::Operation)
}

pub(super) fn copy_index_snapshot(
    index_path: &Path,
    destination: &mut fs::File,
    preserve_permissions: bool,
) -> Result<(), LocalGitFailure> {
    match openat(
        CWD,
        index_path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            let mut source = fs::File::from(descriptor);
            let metadata = source.metadata().map_err(|_| LocalGitFailure::Repository)?;
            if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES as u64 {
                return Err(LocalGitFailure::Repository);
            }
            if preserve_permissions {
                destination
                    .set_permissions(metadata.permissions())
                    .map_err(|_| LocalGitFailure::Operation)?;
            }
            let copied = std::io::copy(
                &mut Read::by_ref(&mut source).take((MAX_INDEX_BYTES + 1) as u64),
                destination,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            if copied > MAX_INDEX_BYTES as u64 {
                return Err(LocalGitFailure::Repository);
            }
        }
        Err(rustix::io::Errno::NOENT) => write_empty_index(destination)?,
        Err(_) => return Err(LocalGitFailure::Repository),
    }
    Ok(())
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if !self.committed {
            remove_owned_index_lock(&self.lock_path, &self.lock, self.identity);
        }
    }
}

pub(super) fn remove_owned_index_lock(lock_path: &Path, lock: &fs::File, identity: FileIdentity) {
    let path_identity = fs::symlink_metadata(lock_path)
        .map(|metadata| file_identity(&metadata))
        .ok();
    let descriptor_identity = lock
        .metadata()
        .map(|metadata| file_identity(&metadata))
        .ok();
    if path_identity == Some(identity) && descriptor_identity == Some(identity) {
        let _ = fs::remove_file(lock_path);
    }
}

pub(super) fn write_empty_index(file: &mut fs::File) -> Result<(), LocalGitFailure> {
    const EMPTY_INDEX_HEADER: &[u8; 12] = b"DIRC\0\0\0\x02\0\0\0\0";
    file.write_all(EMPTY_INDEX_HEADER)
        .map_err(|_| LocalGitFailure::Operation)?;
    file.write_all(&Sha1::digest(EMPTY_INDEX_HEADER))
        .map_err(|_| LocalGitFailure::Operation)
}
