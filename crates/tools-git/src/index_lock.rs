use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Seek, Write},
    os::fd::OwnedFd,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use git2::Index;
use rustix::{
    fs::{AtFlags, CWD, Mode, OFlags, RenameFlags, openat, renameat_with, unlinkat},
    io::dup,
};
use sha1::{Digest, Sha1};

use crate::descriptor::{FileIdentity, descriptor_path, file_identity};
use crate::failure::LocalGitFailure;
use crate::limits::MAX_INDEX_BYTES;
use crate::pinning::PinnedRepository;

pub(super) struct IndexLock {
    parent: OwnedFd,
    index_name: OsString,
    lock_name: OsString,
    lock: fs::File,
    identity: FileIdentity,
    expected_index: Option<IndexSnapshotIdentity>,
    _private_directory: tempfile::TempDir,
    private_index_path: PathBuf,
    committed: bool,
}

pub(super) struct IndexSnapshot {
    _file: fs::File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IndexSnapshotIdentity {
    file: FileIdentity,
    length: u64,
    digest: [u8; 20],
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
        Self::acquire_at_with_private_directory_and_mode(
            dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?,
            OsString::from("index"),
            OsString::from("index.lock"),
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
        if index_path.parent() != lock_path.parent() {
            return Err(LocalGitFailure::Operation);
        }
        let parent_path = lock_path.parent().ok_or(LocalGitFailure::Operation)?;
        let parent = openat(
            CWD,
            parent_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let index_name = index_path
            .file_name()
            .ok_or(LocalGitFailure::Operation)?
            .to_owned();
        let lock_name = lock_path
            .file_name()
            .ok_or(LocalGitFailure::Operation)?
            .to_owned();
        Self::acquire_at_with_private_directory_and_mode(
            parent,
            index_name,
            lock_name,
            missing_index_mode,
            create_private_directory,
        )
    }

    fn acquire_at_with_private_directory_and_mode<Create>(
        parent: OwnedFd,
        index_name: OsString,
        lock_name: OsString,
        missing_index_mode: Mode,
        create_private_directory: Create,
    ) -> Result<(Self, Index), LocalGitFailure>
    where
        Create: FnOnce() -> std::io::Result<tempfile::TempDir>,
    {
        let descriptor = openat(
            &parent,
            &lock_name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let lock = fs::File::from(descriptor);
        let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        let private_directory = match create_private_directory() {
            Ok(directory) => directory,
            Err(_) => {
                remove_owned_index_lock(&parent, &lock_name, &lock, identity);
                return Err(LocalGitFailure::Operation);
            }
        };
        let private_index_path = private_directory.path().join("index");
        let mut guard = Self {
            parent,
            index_name,
            lock_name,
            lock,
            identity,
            expected_index: None,
            _private_directory: private_directory,
            private_index_path,
            committed: false,
        };
        guard
            .lock
            .set_permissions(fs::Permissions::from_mode(missing_index_mode.bits()))
            .map_err(|_| LocalGitFailure::Operation)?;
        guard.expected_index =
            copy_index_snapshot_at(&guard.parent, &guard.index_name, &mut guard.lock, true)?;
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

    pub(super) fn commit(self) -> Result<FileIdentity, LocalGitFailure> {
        self.commit_with_hook(|| {})
    }

    #[cfg(test)]
    pub(super) fn commit_with_test_hook<Hook: FnOnce()>(
        self,
        before_publish: Hook,
    ) -> Result<FileIdentity, LocalGitFailure> {
        self.commit_with_hook(before_publish)
    }

    fn commit_with_hook<Hook: FnOnce()>(
        mut self,
        before_publish: Hook,
    ) -> Result<FileIdentity, LocalGitFailure> {
        let path_identity =
            entry_identity(&self.parent, &self.lock_name)?.ok_or(LocalGitFailure::Operation)?;
        let descriptor_identity = file_identity(
            &self
                .lock
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?,
        );
        if path_identity != self.identity || descriptor_identity != self.identity {
            return Err(LocalGitFailure::Operation);
        }
        before_publish();
        match self.expected_index {
            Some(original_index) => {
                if index_snapshot_identity_at(&self.parent, &self.index_name)?
                    != Some(original_index)
                {
                    return Err(LocalGitFailure::Operation);
                }
                renameat_with(
                    &self.parent,
                    &self.lock_name,
                    &self.parent,
                    &self.index_name,
                    RenameFlags::EXCHANGE,
                )
                .map_err(|_| LocalGitFailure::Operation)?;
                let publication_is_owned =
                    entry_identity(&self.parent, &self.index_name) == Ok(Some(self.identity));
                let displaced_is_current =
                    index_snapshot_identity_at(&self.parent, &self.lock_name)
                        == Ok(Some(original_index));
                if !publication_is_owned || !displaced_is_current {
                    if publication_is_owned || displaced_is_current {
                        let _ = renameat_with(
                            &self.parent,
                            &self.lock_name,
                            &self.parent,
                            &self.index_name,
                            RenameFlags::EXCHANGE,
                        );
                    }
                    return Err(LocalGitFailure::Operation);
                }
                if unlinkat(&self.parent, &self.lock_name, AtFlags::empty()).is_err() {
                    let _ = renameat_with(
                        &self.parent,
                        &self.lock_name,
                        &self.parent,
                        &self.index_name,
                        RenameFlags::EXCHANGE,
                    );
                    return Err(LocalGitFailure::Operation);
                }
            }
            None => {
                if entry_identity(&self.parent, &self.index_name)?.is_some() {
                    return Err(LocalGitFailure::Operation);
                }
                renameat_with(
                    &self.parent,
                    &self.lock_name,
                    &self.parent,
                    &self.index_name,
                    RenameFlags::NOREPLACE,
                )
                .map_err(|_| LocalGitFailure::Operation)?;
                if entry_identity(&self.parent, &self.index_name) != Ok(Some(self.identity)) {
                    let _ = renameat_with(
                        &self.parent,
                        &self.index_name,
                        &self.parent,
                        &self.lock_name,
                        RenameFlags::NOREPLACE,
                    );
                    return Err(LocalGitFailure::Operation);
                }
            }
        }
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
            copy_open_index_snapshot(descriptor, destination, preserve_permissions).map(drop)?
        }
        Err(rustix::io::Errno::NOENT) => write_empty_index(destination)?,
        Err(_) => return Err(LocalGitFailure::Repository),
    }
    Ok(())
}

fn copy_open_index_snapshot(
    descriptor: OwnedFd,
    destination: &mut fs::File,
    preserve_permissions: bool,
) -> Result<IndexSnapshotIdentity, LocalGitFailure> {
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
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut source)
        .take((MAX_INDEX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    if bytes.len() as u64 != metadata.len() || bytes.len() > MAX_INDEX_BYTES {
        return Err(LocalGitFailure::Repository);
    }
    destination
        .write_all(&bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    Ok(IndexSnapshotIdentity {
        file: file_identity(&metadata),
        length: metadata.len(),
        digest: Sha1::digest(&bytes).into(),
    })
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if !self.committed {
            remove_owned_index_lock(&self.parent, &self.lock_name, &self.lock, self.identity);
        }
    }
}

pub(super) fn remove_owned_index_lock(
    parent: &OwnedFd,
    lock_name: &OsStr,
    lock: &fs::File,
    identity: FileIdentity,
) {
    let path_identity = entry_identity(parent, lock_name).ok().flatten();
    let descriptor_identity = lock
        .metadata()
        .map(|metadata| file_identity(&metadata))
        .ok();
    if path_identity == Some(identity) && descriptor_identity == Some(identity) {
        let _ = unlinkat(parent, lock_name, AtFlags::empty());
    }
}

fn entry_identity(parent: &OwnedFd, name: &OsStr) -> Result<Option<FileIdentity>, LocalGitFailure> {
    match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => fs::File::from(descriptor)
            .metadata()
            .map(|metadata| Some(file_identity(&metadata)))
            .map_err(|_| LocalGitFailure::Operation),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
        Err(_) => Err(LocalGitFailure::Operation),
    }
}

fn copy_index_snapshot_at(
    parent: &OwnedFd,
    index_name: &OsStr,
    destination: &mut fs::File,
    preserve_permissions: bool,
) -> Result<Option<IndexSnapshotIdentity>, LocalGitFailure> {
    match openat(
        parent,
        index_name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            copy_open_index_snapshot(descriptor, destination, preserve_permissions).map(Some)
        }
        Err(rustix::io::Errno::NOENT) => {
            write_empty_index(destination)?;
            Ok(None)
        }
        Err(_) => Err(LocalGitFailure::Repository),
    }
}

fn index_snapshot_identity_at(
    parent: &OwnedFd,
    name: &OsStr,
) -> Result<Option<IndexSnapshotIdentity>, LocalGitFailure> {
    match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            let mut file = fs::File::from(descriptor);
            let metadata = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
            snapshot_identity(&mut file, &metadata).map(Some)
        }
        Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
        Err(_) => Err(LocalGitFailure::Operation),
    }
}

fn snapshot_identity(
    file: &mut fs::File,
    metadata: &fs::Metadata,
) -> Result<IndexSnapshotIdentity, LocalGitFailure> {
    if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES as u64 {
        return Err(LocalGitFailure::Operation);
    }
    file.rewind().map_err(|_| LocalGitFailure::Operation)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(file)
        .take((MAX_INDEX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    if bytes.len() as u64 != metadata.len() || bytes.len() > MAX_INDEX_BYTES {
        return Err(LocalGitFailure::Operation);
    }
    Ok(IndexSnapshotIdentity {
        file: file_identity(metadata),
        length: metadata.len(),
        digest: Sha1::digest(&bytes).into(),
    })
}

pub(super) fn write_empty_index(file: &mut fs::File) -> Result<(), LocalGitFailure> {
    const EMPTY_INDEX_HEADER: &[u8; 12] = b"DIRC\0\0\0\x02\0\0\0\0";
    file.write_all(EMPTY_INDEX_HEADER)
        .map_err(|_| LocalGitFailure::Operation)?;
    file.write_all(&Sha1::digest(EMPTY_INDEX_HEADER))
        .map_err(|_| LocalGitFailure::Operation)
}
