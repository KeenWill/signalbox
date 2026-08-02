use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Seek, Write},
    os::fd::OwnedFd,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use git2::{Index, ObjectFormat};
use rustix::{
    fs::{AtFlags, CWD, Mode, OFlags, RenameFlags, openat, renameat_with, unlinkat},
    io::dup,
};
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::descriptor::{
    FileIdentity, QuarantineDirectory, descriptor_path, file_identity, remove_entry_if_identity,
};
use crate::failure::LocalGitFailure;
use crate::limits::{MAX_INDEX_BYTES, MAX_INDEX_ENTRIES};
use crate::pinning::PinnedRepository;

pub(super) struct IndexLock {
    parent: OwnedFd,
    index_name: OsString,
    lock_name: OsString,
    lock: fs::File,
    identity: FileIdentity,
    object_format: ObjectFormat,
    expected_index: Option<IndexSnapshotIdentity>,
    prepared_index: Option<IndexSnapshotIdentity>,
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
    digest: [u8; 32],
}

impl IndexSnapshot {
    pub(super) fn acquire(
        index_path: &Path,
        object_format: ObjectFormat,
    ) -> Result<(Self, Index), LocalGitFailure> {
        let mut file = tempfile::tempfile().map_err(|_| LocalGitFailure::Operation)?;
        copy_index_snapshot(index_path, &mut file, false, object_format)?;
        let index = Index::open_ext(&descriptor_path(&file), object_format)
            .map_err(|_| LocalGitFailure::Operation)?;
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
            authority.object_format,
            tempfile::tempdir,
            || {},
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
            ObjectFormat::Sha1,
            tempfile::tempdir,
            || {},
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
            ObjectFormat::Sha1,
            create_private_directory,
            || {},
        )
    }

    #[cfg(test)]
    pub(super) fn acquire_with_preclone_hook<AfterPrepare: FnOnce()>(
        index_path: &Path,
        lock_path: &Path,
        after_prepare: AfterPrepare,
    ) -> Result<(Self, Index), LocalGitFailure> {
        Self::acquire_with_private_directory_and_mode(
            index_path,
            lock_path,
            Mode::RUSR | Mode::WUSR,
            ObjectFormat::Sha1,
            tempfile::tempdir,
            after_prepare,
        )
    }

    fn acquire_with_private_directory_and_mode<Create, AfterPrepare>(
        index_path: &Path,
        lock_path: &Path,
        missing_index_mode: Mode,
        object_format: ObjectFormat,
        create_private_directory: Create,
        after_prepare: AfterPrepare,
    ) -> Result<(Self, Index), LocalGitFailure>
    where
        Create: FnOnce() -> std::io::Result<tempfile::TempDir>,
        AfterPrepare: FnOnce(),
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
            object_format,
            create_private_directory,
            after_prepare,
        )
    }

    fn acquire_at_with_private_directory_and_mode<Create, AfterPrepare>(
        parent: OwnedFd,
        index_name: OsString,
        lock_name: OsString,
        missing_index_mode: Mode,
        object_format: ObjectFormat,
        create_private_directory: Create,
        after_prepare: AfterPrepare,
    ) -> Result<(Self, Index), LocalGitFailure>
    where
        Create: FnOnce() -> std::io::Result<tempfile::TempDir>,
        AfterPrepare: FnOnce(),
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
            object_format,
            expected_index: None,
            prepared_index: None,
            _private_directory: private_directory,
            private_index_path,
            committed: false,
        };
        guard
            .lock
            .set_permissions(fs::Permissions::from_mode(missing_index_mode.bits()))
            .map_err(|_| LocalGitFailure::Operation)?;
        guard.expected_index = copy_index_snapshot_at(
            &guard.parent,
            &guard.index_name,
            &mut guard.lock,
            true,
            guard.object_format,
        )?;
        guard
            .lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        guard.record_prepared_index()?;
        after_prepare();
        guard.copy_lock_to_private_index()?;
        let index = Index::open_ext(&guard.private_index_path, guard.object_format)
            .map_err(|_| LocalGitFailure::Operation)?;
        Ok((guard, index))
    }

    fn copy_lock_to_private_index(&mut self) -> Result<(), LocalGitFailure> {
        let prepared = self.prepared_index.ok_or(LocalGitFailure::Operation)?;
        if self.lock_snapshot_identity()? != prepared
            || index_snapshot_identity_at(&self.parent, &self.lock_name)? != Some(prepared)
        {
            return Err(LocalGitFailure::Operation);
        }
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
            .map_err(|_| LocalGitFailure::Operation)?;
        let private_metadata = private_index
            .metadata()
            .map_err(|_| LocalGitFailure::Operation)?;
        let private_snapshot = snapshot_identity(&mut private_index, &private_metadata)?;
        if private_snapshot.length != prepared.length
            || private_snapshot.digest != prepared.digest
            || self.lock_snapshot_identity()? != prepared
            || index_snapshot_identity_at(&self.parent, &self.lock_name)? != Some(prepared)
        {
            return Err(LocalGitFailure::Operation);
        }
        Ok(())
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
            .map_err(|_| LocalGitFailure::Operation)?;
        self.record_prepared_index()
    }

    pub(super) fn write(&mut self, index: &mut Index) -> Result<(), LocalGitFailure> {
        if index.path() != Some(self.private_index_path.as_path()) {
            write_index_entries(&mut self.lock, index, self.object_format)?;
            return self.record_prepared_index();
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
        self.lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        self.record_prepared_index()
    }

    pub(super) fn commit(self) -> Result<FileIdentity, LocalGitFailure> {
        self.commit_with_hooks(|| {}, || {}, || {})
    }

    #[cfg(test)]
    pub(super) fn commit_with_test_hook<Hook: FnOnce()>(
        self,
        before_publish: Hook,
    ) -> Result<FileIdentity, LocalGitFailure> {
        self.commit_with_hooks(before_publish, || {}, || {})
    }

    #[cfg(test)]
    pub(super) fn commit_with_exchange_test_hook<Hook: FnOnce()>(
        self,
        after_exchange: Hook,
    ) -> Result<FileIdentity, LocalGitFailure> {
        self.commit_with_hooks(|| {}, after_exchange, || {})
    }

    #[cfg(test)]
    pub(super) fn commit_with_cleanup_test_hook<Hook: FnOnce()>(
        self,
        before_cleanup: Hook,
    ) -> Result<FileIdentity, LocalGitFailure> {
        self.commit_with_hooks(|| {}, || {}, before_cleanup)
    }

    fn commit_with_hooks<
        BeforePublish: FnOnce(),
        AfterExchange: FnOnce(),
        BeforeCleanup: FnOnce(),
    >(
        mut self,
        before_publish: BeforePublish,
        after_exchange: AfterExchange,
        before_cleanup: BeforeCleanup,
    ) -> Result<FileIdentity, LocalGitFailure> {
        let prepared_index = self.prepared_index.ok_or(LocalGitFailure::Operation)?;
        before_publish();
        if self.lock_snapshot_identity()? != prepared_index
            || index_snapshot_identity_at(&self.parent, &self.lock_name)? != Some(prepared_index)
        {
            return Err(LocalGitFailure::Operation);
        }
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
                after_exchange();
                let publication_is_owned =
                    index_snapshot_identity_at(&self.parent, &self.index_name)
                        == Ok(Some(prepared_index));
                let displaced_is_current =
                    index_snapshot_identity_at(&self.parent, &self.lock_name)
                        == Ok(Some(original_index));
                if !publication_is_owned || !displaced_is_current {
                    if publication_is_owned && displaced_is_current {
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
                before_cleanup();
                if remove_displaced_index_if_current(&self.parent, &self.lock_name, original_index)
                    .is_err()
                {
                    return Err(LocalGitFailure::Operation);
                }
                if index_snapshot_identity_at(&self.parent, &self.index_name)
                    != Ok(Some(prepared_index))
                {
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
                if index_snapshot_identity_at(&self.parent, &self.index_name)
                    != Ok(Some(prepared_index))
                {
                    return Err(LocalGitFailure::Operation);
                }
            }
        }
        self.committed = true;
        Ok(self.identity)
    }

    fn record_prepared_index(&mut self) -> Result<(), LocalGitFailure> {
        let snapshot = self.lock_snapshot_identity()?;
        if snapshot.file != self.identity {
            return Err(LocalGitFailure::Operation);
        }
        self.prepared_index = Some(snapshot);
        Ok(())
    }

    fn lock_snapshot_identity(&mut self) -> Result<IndexSnapshotIdentity, LocalGitFailure> {
        let metadata = self
            .lock
            .metadata()
            .map_err(|_| LocalGitFailure::Operation)?;
        snapshot_identity(&mut self.lock, &metadata)
    }
}

fn remove_displaced_index_if_current(
    parent: &OwnedFd,
    name: &OsStr,
    expected: IndexSnapshotIdentity,
) -> Result<(), LocalGitFailure> {
    let quarantine = QuarantineDirectory::create(parent)?;
    let quarantined_name = OsStr::new("displaced");
    renameat_with(
        parent,
        name,
        quarantine.descriptor(),
        quarantined_name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let quarantined_is_expected =
        index_snapshot_identity_at(quarantine.descriptor(), quarantined_name) == Ok(Some(expected));
    if !quarantined_is_expected {
        let _ = renameat_with(
            quarantine.descriptor(),
            quarantined_name,
            parent,
            name,
            RenameFlags::NOREPLACE,
        );
        return Err(LocalGitFailure::Operation);
    }
    unlinkat(quarantine.descriptor(), quarantined_name, AtFlags::empty())
        .map_err(|_| LocalGitFailure::Operation)?;
    if entry_identity(parent, name)?.is_some() {
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
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
    object_format: ObjectFormat,
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
    append_index_checksum(&mut bytes, object_format);
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
    object_format: ObjectFormat,
) -> Result<(), LocalGitFailure> {
    match openat(
        CWD,
        index_path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            copy_open_index_snapshot(descriptor, destination, preserve_permissions, object_format)
                .map(drop)?
        }
        Err(rustix::io::Errno::NOENT) => write_empty_index(destination, object_format)?,
        Err(_) => return Err(LocalGitFailure::Repository),
    }
    Ok(())
}

fn copy_open_index_snapshot(
    descriptor: OwnedFd,
    destination: &mut fs::File,
    preserve_permissions: bool,
    object_format: ObjectFormat,
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
    reject_split_index(&bytes, object_format)?;
    destination
        .write_all(&bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    Ok(IndexSnapshotIdentity {
        file: file_identity(&metadata),
        length: metadata.len(),
        digest: Sha256::digest(&bytes).into(),
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
    let descriptor_identity = lock
        .metadata()
        .map(|metadata| file_identity(&metadata))
        .ok();
    if descriptor_identity == Some(identity) {
        let _ = remove_entry_if_identity(parent, lock_name, identity, AtFlags::empty());
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
    object_format: ObjectFormat,
) -> Result<Option<IndexSnapshotIdentity>, LocalGitFailure> {
    match openat(
        parent,
        index_name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            copy_open_index_snapshot(descriptor, destination, preserve_permissions, object_format)
                .map(Some)
        }
        Err(rustix::io::Errno::NOENT) => {
            write_empty_index(destination, object_format)?;
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
        digest: Sha256::digest(&bytes).into(),
    })
}

fn reject_split_index(bytes: &[u8], object_format: ObjectFormat) -> Result<(), LocalGitFailure> {
    let object_id_bytes = match object_format {
        ObjectFormat::Sha1 => 20,
        ObjectFormat::Sha256 => 32,
    };
    let extension_end = bytes
        .len()
        .checked_sub(object_id_bytes)
        .filter(|end| *end >= 12)
        .ok_or(LocalGitFailure::Repository)?;
    if &bytes[..4] != b"DIRC" {
        return Err(LocalGitFailure::Repository);
    }
    let version = u32::from_be_bytes(
        bytes[4..8]
            .try_into()
            .map_err(|_| LocalGitFailure::Repository)?,
    );
    if !(2..=4).contains(&version) {
        return Err(LocalGitFailure::Repository);
    }
    let entries = usize::try_from(u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| LocalGitFailure::Repository)?,
    ))
    .map_err(|_| LocalGitFailure::Repository)?;
    if entries > MAX_INDEX_ENTRIES {
        return Err(LocalGitFailure::Repository);
    }
    let mut cursor = 12_usize;
    for _entry in 0..entries {
        let entry_start = cursor;
        let flags_offset = cursor
            .checked_add(40 + object_id_bytes)
            .filter(|offset| offset.saturating_add(2) <= extension_end)
            .ok_or(LocalGitFailure::Repository)?;
        let flags = u16::from_be_bytes(
            bytes[flags_offset..flags_offset + 2]
                .try_into()
                .map_err(|_| LocalGitFailure::Repository)?,
        );
        cursor = flags_offset + 2;
        if flags & 0x4000 != 0 {
            cursor = cursor
                .checked_add(2)
                .filter(|cursor| *cursor <= extension_end)
                .ok_or(LocalGitFailure::Repository)?;
        }
        if version == 4 {
            let mut prefix_bytes = 0_usize;
            loop {
                let byte = *bytes.get(cursor).ok_or(LocalGitFailure::Repository)?;
                cursor += 1;
                prefix_bytes += 1;
                if byte & 0x80 == 0 {
                    break;
                }
                if prefix_bytes == 10 {
                    return Err(LocalGitFailure::Repository);
                }
            }
            let suffix = bytes
                .get(cursor..extension_end)
                .ok_or(LocalGitFailure::Repository)?;
            let nul = suffix
                .iter()
                .position(|byte| *byte == 0)
                .ok_or(LocalGitFailure::Repository)?;
            cursor = cursor
                .checked_add(nul + 1)
                .ok_or(LocalGitFailure::Repository)?;
        } else {
            let stated_path_bytes = usize::from(flags & 0x0fff);
            if stated_path_bytes < 0x0fff {
                let nul = cursor
                    .checked_add(stated_path_bytes)
                    .filter(|nul| bytes.get(*nul) == Some(&0))
                    .ok_or(LocalGitFailure::Repository)?;
                cursor = nul + 1;
            } else {
                let path = bytes
                    .get(cursor..extension_end)
                    .ok_or(LocalGitFailure::Repository)?;
                let nul = path
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or(LocalGitFailure::Repository)?;
                cursor = cursor
                    .checked_add(nul + 1)
                    .ok_or(LocalGitFailure::Repository)?;
            }
            let entry_bytes = cursor
                .checked_sub(entry_start)
                .ok_or(LocalGitFailure::Repository)?;
            cursor = entry_start
                .checked_add((entry_bytes + 7) & !7)
                .filter(|cursor| *cursor <= extension_end)
                .ok_or(LocalGitFailure::Repository)?;
        }
    }
    while cursor < extension_end {
        let header_end = cursor
            .checked_add(8)
            .filter(|end| *end <= extension_end)
            .ok_or(LocalGitFailure::Repository)?;
        let signature = &bytes[cursor..cursor + 4];
        let extension_bytes = usize::try_from(u32::from_be_bytes(
            bytes[cursor + 4..header_end]
                .try_into()
                .map_err(|_| LocalGitFailure::Repository)?,
        ))
        .map_err(|_| LocalGitFailure::Repository)?;
        if signature == b"link" {
            return Err(LocalGitFailure::Repository);
        }
        cursor = header_end
            .checked_add(extension_bytes)
            .filter(|cursor| *cursor <= extension_end)
            .ok_or(LocalGitFailure::Repository)?;
    }
    Ok(())
}

pub(super) fn write_empty_index(
    file: &mut fs::File,
    object_format: ObjectFormat,
) -> Result<(), LocalGitFailure> {
    const EMPTY_INDEX_HEADER: &[u8; 12] = b"DIRC\0\0\0\x02\0\0\0\0";
    file.write_all(EMPTY_INDEX_HEADER)
        .map_err(|_| LocalGitFailure::Operation)?;
    match object_format {
        ObjectFormat::Sha1 => file.write_all(&Sha1::digest(EMPTY_INDEX_HEADER)),
        ObjectFormat::Sha256 => file.write_all(&Sha256::digest(EMPTY_INDEX_HEADER)),
    }
    .map_err(|_| LocalGitFailure::Operation)
}

fn append_index_checksum(bytes: &mut Vec<u8>, object_format: ObjectFormat) {
    match object_format {
        ObjectFormat::Sha1 => bytes.extend_from_slice(&Sha1::digest(&*bytes)),
        ObjectFormat::Sha256 => bytes.extend_from_slice(&Sha256::digest(&*bytes)),
    }
}
