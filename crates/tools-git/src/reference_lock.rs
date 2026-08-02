use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Seek, Write},
    os::{
        fd::OwnedFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
};

use rustix::{
    fs::{
        AtFlags, FileType, Mode, OFlags, RenameFlags, mkdirat, openat, renameat_with, statat,
        unlinkat,
    },
    io::dup,
};

use crate::descriptor::{
    FileIdentity, QuarantineDirectory, descriptor_entry_exists, file_identity,
    remove_entry_if_identity,
};
use crate::failure::LocalGitFailure;
use crate::limits::MAX_REVISION_BYTES;
use crate::packed_reference::{packed_reference_namespace_conflicts, packed_reference_target};
use crate::pinning::PinnedRepository;
use crate::reference_read::{open_git_directory_path, read_reference_leaf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PinnedReferenceValue {
    Direct(git2::Oid),
    Symbolic(String),
    Missing,
}

pub(super) struct ReferenceLock {
    pub(super) name: String,
    parent: OwnedFd,
    leaf: OsString,
    lock_name: OsString,
    lock: fs::File,
    identity: FileIdentity,
    prepared: Option<ReferenceSnapshotIdentity>,
    hierarchy: Vec<(PathBuf, FileIdentity)>,
    _created_directories: CreatedReferenceDirectories,
    committed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReferenceSnapshotIdentity {
    file: FileIdentity,
    length: u64,
    digest: [u8; 32],
}

pub(super) struct ReferenceParent {
    pub(super) directory: OwnedFd,
    pub(super) leaf: OsString,
    hierarchy: Vec<(PathBuf, FileIdentity)>,
    created_directories: CreatedReferenceDirectories,
    creation_file_mode: Option<Mode>,
}

#[derive(Debug)]
pub(super) struct CreatedReferenceDirectory {
    parent: OwnedFd,
    name: OsString,
    identity: FileIdentity,
}

#[derive(Default)]
pub(super) struct CreatedReferenceDirectories(Vec<CreatedReferenceDirectory>);

impl ReferenceLock {
    pub(super) fn acquire(
        authority: &PinnedRepository,
        name: &str,
    ) -> Result<Self, LocalGitFailure> {
        let bound = open_reference_parent(authority, name, true)?;
        let creation_file_mode = bound.creation_file_mode;
        let parent = bound.directory;
        let leaf = bound.leaf;
        let mut lock_name = OsString::from(&leaf);
        lock_name.push(".lock");
        let descriptor = openat(
            &parent,
            &lock_name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let lock = fs::File::from(descriptor);
        let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        let guard = Self {
            name: name.to_owned(),
            parent,
            leaf,
            lock_name,
            lock,
            identity,
            prepared: None,
            hierarchy: bound.hierarchy,
            _created_directories: bound.created_directories,
            committed: false,
        };
        let permissions = reference_permissions(&guard.parent, &guard.leaf)?
            .or_else(|| creation_file_mode.map(|mode| fs::Permissions::from_mode(mode.bits())));
        if let Some(permissions) = permissions {
            guard
                .lock
                .set_permissions(permissions)
                .map_err(|_| LocalGitFailure::Operation)?;
        }
        Ok(guard)
    }

    pub(super) fn read(
        &self,
        authority: &PinnedRepository,
    ) -> Result<PinnedReferenceValue, LocalGitFailure> {
        read_reference_leaf(&self.parent, &self.leaf, authority, &self.name)
    }

    pub(super) fn hierarchy_is_current(&self, authority: &PinnedRepository) -> bool {
        self.hierarchy.iter().all(|(relative, expected)| {
            open_git_directory_path(authority, relative)
                .and_then(|directory| {
                    let metadata = fs::File::from(directory)
                        .metadata()
                        .map_err(|_| LocalGitFailure::Operation)?;
                    Ok(file_identity(&metadata) == *expected)
                })
                .unwrap_or(false)
        })
    }

    pub(super) fn prepare(
        &mut self,
        authority: &PinnedRepository,
        target: git2::Oid,
    ) -> Result<(), LocalGitFailure> {
        self.lock
            .set_len(0)
            .and_then(|()| self.lock.rewind())
            .map_err(|_| LocalGitFailure::Operation)?;
        writeln!(self.lock, "{target}").map_err(|_| LocalGitFailure::Operation)?;
        self.lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        self.record_prepared_reference()?;
        if !self.path_still_owned() || !self.hierarchy_is_current(authority) {
            return Err(LocalGitFailure::Operation);
        }
        Ok(())
    }

    pub(super) fn prepare_symbolic(
        &mut self,
        authority: &PinnedRepository,
        target: &str,
    ) -> Result<(), LocalGitFailure> {
        if !target.starts_with("refs/") || !git2::Reference::is_valid_name(target) {
            return Err(LocalGitFailure::Operation);
        }
        self.lock
            .set_len(0)
            .and_then(|()| self.lock.rewind())
            .map_err(|_| LocalGitFailure::Operation)?;
        writeln!(self.lock, "ref: {target}").map_err(|_| LocalGitFailure::Operation)?;
        self.lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        self.record_prepared_reference()?;
        if !self.path_still_owned() || !self.hierarchy_is_current(authority) {
            return Err(LocalGitFailure::Operation);
        }
        Ok(())
    }

    pub(super) fn publish(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hook(authority, expected, || {})
    }

    pub(super) fn publish_with_hook<Hook: FnOnce()>(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        before_absent_publish: Hook,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hooks(
            authority,
            expected,
            before_absent_publish,
            || {},
            || {},
            || {},
        )
    }

    fn publish_with_hooks<
        BeforeAbsent: FnOnce(),
        AfterPublish: FnOnce(),
        BeforeCleanup: FnOnce(),
        BeforeRollback: FnOnce(),
    >(
        mut self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        before_absent_publish: BeforeAbsent,
        after_publish: AfterPublish,
        before_cleanup: BeforeCleanup,
        before_rollback: BeforeRollback,
    ) -> Result<(), LocalGitFailure> {
        let mut before_rollback = Some(before_rollback);
        if !self.path_still_owned()
            || !self.prepared_lock_is_current()
            || !self.hierarchy_is_current(authority)
        {
            return Err(LocalGitFailure::Operation);
        }
        let expected_packed = packed_reference_target(authority, &self.name)?;
        if packed_reference_namespace_conflicts(authority, &self.name)? {
            return Err(LocalGitFailure::Operation);
        }
        if !descriptor_entry_exists(&self.parent, &self.leaf)? {
            if self.read(authority)? != *expected {
                return Err(LocalGitFailure::Operation);
            }
            before_absent_publish();
            if !self.prepared_lock_is_current() {
                return Err(LocalGitFailure::Operation);
            }
            renameat_with(
                &self.parent,
                &self.lock_name,
                &self.parent,
                &self.leaf,
                RenameFlags::NOREPLACE,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            after_publish();
            let packed_is_current = packed_reference_target(authority, &self.name)
                .is_ok_and(|current| current == expected_packed);
            let packed_namespace_is_clear =
                packed_reference_namespace_conflicts(authority, &self.name)
                    .is_ok_and(|conflicts| !conflicts);
            let publication_is_current = self.prepared_publication_is_current();
            if !publication_is_current
                || !packed_is_current
                || !packed_namespace_is_clear
                || !self.hierarchy_is_current(authority)
            {
                before_rollback.take().ok_or(LocalGitFailure::Operation)?();
                if publication_is_current {
                    let prepared = self.prepared.ok_or(LocalGitFailure::Operation)?;
                    let _ =
                        remove_displaced_reference_if_current(&self.parent, &self.leaf, prepared);
                }
                return Err(LocalGitFailure::Operation);
            }
            self.committed = true;
            return Ok(());
        }
        let expected_leaf_snapshot = reference_snapshot_identity_at(&self.parent, &self.leaf)?
            .ok_or(LocalGitFailure::Operation)?;
        renameat_with(
            &self.parent,
            &self.lock_name,
            &self.parent,
            &self.leaf,
            RenameFlags::EXCHANGE,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        after_publish();
        let displaced = read_reference_leaf(&self.parent, &self.lock_name, authority, &self.name);
        let packed_is_current = packed_reference_target(authority, &self.name)
            .is_ok_and(|current| current == expected_packed);
        let packed_namespace_is_clear = packed_reference_namespace_conflicts(authority, &self.name)
            .is_ok_and(|conflicts| !conflicts);
        let displaced_value_is_expected = displaced.as_ref() == Ok(expected);
        let displaced_snapshot_is_current =
            reference_snapshot_identity_at(&self.parent, &self.lock_name)
                == Ok(Some(expected_leaf_snapshot));
        let publication_is_current = self.prepared_publication_is_current();
        if !displaced_value_is_expected
            || !displaced_snapshot_is_current
            || !publication_is_current
            || !packed_is_current
            || !packed_namespace_is_clear
            || !self.hierarchy_is_current(authority)
        {
            before_rollback.take().ok_or(LocalGitFailure::Operation)?();
            if displaced_snapshot_is_current && publication_is_current {
                let _ = rollback_reference_exchange_if_current(
                    &self.parent,
                    &self.leaf,
                    &self.lock_name,
                    expected_leaf_snapshot,
                    self.prepared.ok_or(LocalGitFailure::Operation)?,
                );
            }
            return Err(LocalGitFailure::Operation);
        }
        before_cleanup();
        if remove_displaced_reference_if_current(
            &self.parent,
            &self.lock_name,
            expected_leaf_snapshot,
        )
        .is_err()
        {
            return Err(LocalGitFailure::Operation);
        }
        if !self.prepared_publication_is_current() {
            return Err(LocalGitFailure::Operation);
        }
        self.committed = true;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn publish_with_test_hooks<BeforeAbsent: FnOnce(), AfterPublish: FnOnce()>(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        before_absent_publish: BeforeAbsent,
        after_publish: AfterPublish,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hooks(
            authority,
            expected,
            before_absent_publish,
            after_publish,
            || {},
            || {},
        )
    }

    #[cfg(test)]
    pub(super) fn publish_with_cleanup_test_hook<BeforeCleanup: FnOnce()>(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        before_cleanup: BeforeCleanup,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hooks(authority, expected, || {}, || {}, before_cleanup, || {})
    }

    #[cfg(test)]
    pub(super) fn publish_with_rollback_test_hook<
        AfterPublish: FnOnce(),
        BeforeRollback: FnOnce(),
    >(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        after_publish: AfterPublish,
        before_rollback: BeforeRollback,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hooks(
            authority,
            expected,
            || {},
            after_publish,
            || {},
            before_rollback,
        )
    }

    #[cfg(test)]
    pub(super) fn commit(
        mut self,
        authority: &PinnedRepository,
        target: git2::Oid,
    ) -> Result<(), LocalGitFailure> {
        let expected = self.read(authority)?;
        self.prepare(authority, target)?;
        self.publish(authority, &expected)
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

    fn record_prepared_reference(&mut self) -> Result<(), LocalGitFailure> {
        let snapshot = reference_snapshot_identity(&mut self.lock)?;
        if snapshot.file != self.identity {
            return Err(LocalGitFailure::Operation);
        }
        self.prepared = Some(snapshot);
        Ok(())
    }

    fn prepared_lock_is_current(&mut self) -> bool {
        let Some(prepared) = self.prepared else {
            return false;
        };
        reference_snapshot_identity(&mut self.lock).ok() == Some(prepared)
            && reference_snapshot_identity_at(&self.parent, &self.lock_name)
                .ok()
                .flatten()
                == Some(prepared)
    }

    fn prepared_publication_is_current(&mut self) -> bool {
        let Some(prepared) = self.prepared else {
            return false;
        };
        reference_snapshot_identity(&mut self.lock).ok() == Some(prepared)
            && reference_snapshot_identity_at(&self.parent, &self.leaf)
                .ok()
                .flatten()
                == Some(prepared)
    }
}

fn remove_displaced_reference_if_current(
    parent: &OwnedFd,
    name: &OsStr,
    expected: ReferenceSnapshotIdentity,
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
        reference_snapshot_identity_at(quarantine.descriptor(), quarantined_name)
            == Ok(Some(expected));
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
    if descriptor_entry_exists(parent, name)? {
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

fn rollback_reference_exchange_if_current(
    parent: &OwnedFd,
    leaf: &OsStr,
    lock_name: &OsStr,
    displaced: ReferenceSnapshotIdentity,
    publication: ReferenceSnapshotIdentity,
) -> Result<(), LocalGitFailure> {
    let quarantine = QuarantineDirectory::create(parent)?;
    let quarantined_displaced = OsStr::new("displaced");
    let quarantined_publication = OsStr::new("publication");
    renameat_with(
        parent,
        lock_name,
        quarantine.descriptor(),
        quarantined_displaced,
        RenameFlags::NOREPLACE,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    if reference_snapshot_identity_at(quarantine.descriptor(), quarantined_displaced)
        != Ok(Some(displaced))
    {
        let _ = renameat_with(
            quarantine.descriptor(),
            quarantined_displaced,
            parent,
            lock_name,
            RenameFlags::NOREPLACE,
        );
        return Err(LocalGitFailure::Operation);
    }
    if renameat_with(
        parent,
        leaf,
        quarantine.descriptor(),
        quarantined_publication,
        RenameFlags::NOREPLACE,
    )
    .is_err()
    {
        let _ = renameat_with(
            quarantine.descriptor(),
            quarantined_displaced,
            parent,
            lock_name,
            RenameFlags::NOREPLACE,
        );
        return Err(LocalGitFailure::Operation);
    }
    if reference_snapshot_identity_at(quarantine.descriptor(), quarantined_publication)
        != Ok(Some(publication))
    {
        let _ = renameat_with(
            quarantine.descriptor(),
            quarantined_publication,
            parent,
            leaf,
            RenameFlags::NOREPLACE,
        );
        let _ = renameat_with(
            quarantine.descriptor(),
            quarantined_displaced,
            parent,
            lock_name,
            RenameFlags::NOREPLACE,
        );
        return Err(LocalGitFailure::Operation);
    }
    renameat_with(
        quarantine.descriptor(),
        quarantined_displaced,
        parent,
        leaf,
        RenameFlags::NOREPLACE,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    renameat_with(
        quarantine.descriptor(),
        quarantined_publication,
        parent,
        lock_name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|_| LocalGitFailure::Operation)
}

fn reference_snapshot_identity(
    file: &mut fs::File,
) -> Result<ReferenceSnapshotIdentity, LocalGitFailure> {
    let metadata = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if !metadata.is_file() || metadata.len() > MAX_REVISION_BYTES as u64 {
        return Err(LocalGitFailure::Operation);
    }
    file.rewind().map_err(|_| LocalGitFailure::Operation)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(file)
        .take((MAX_REVISION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    if bytes.len() as u64 != metadata.len() || bytes.len() > MAX_REVISION_BYTES {
        return Err(LocalGitFailure::Operation);
    }
    Ok(ReferenceSnapshotIdentity {
        file: file_identity(&metadata),
        length: metadata.len(),
        digest: Sha256::digest(&bytes).into(),
    })
}

fn reference_snapshot_identity_at(
    parent: &OwnedFd,
    name: &OsStr,
) -> Result<Option<ReferenceSnapshotIdentity>, LocalGitFailure> {
    match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => reference_snapshot_identity(&mut fs::File::from(descriptor)).map(Some),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
        Err(_) => Err(LocalGitFailure::Operation),
    }
}

pub(super) fn reference_permissions(
    parent: &OwnedFd,
    leaf: &OsStr,
) -> Result<Option<fs::Permissions>, LocalGitFailure> {
    let descriptor = match openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    let metadata = fs::File::from(descriptor)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    if !metadata.is_file() {
        return Err(LocalGitFailure::Operation);
    }
    Ok(Some(fs::Permissions::from_mode(metadata.mode() & 0o777)))
}

impl Drop for ReferenceLock {
    fn drop(&mut self) {
        let descriptor_is_owned = self
            .lock
            .metadata()
            .map(|metadata| file_identity(&metadata))
            .ok()
            == Some(self.identity);
        if !self.committed && descriptor_is_owned {
            let _ = remove_entry_if_identity(
                &self.parent,
                &self.lock_name,
                self.identity,
                AtFlags::empty(),
            );
        }
    }
}

impl Drop for CreatedReferenceDirectories {
    fn drop(&mut self) {
        for directory in self.0.iter().rev() {
            directory.remove_if_owned();
        }
    }
}

impl CreatedReferenceDirectory {
    fn remove_if_owned(&self) {
        let _ =
            remove_entry_if_identity(&self.parent, &self.name, self.identity, AtFlags::REMOVEDIR);
    }
}

impl CreatedReferenceDirectories {
    pub(super) fn open_or_create(
        &mut self,
        parent: &OwnedFd,
        name: &OsStr,
        mode: Mode,
    ) -> Result<OwnedFd, LocalGitFailure> {
        let (directory, created) =
            open_or_create_ref_directory_with_mode_tracked(parent, name, mode)?;
        if let Some(created) = created {
            self.0.push(created);
        }
        Ok(directory)
    }
}

pub(super) fn open_reference_parent(
    authority: &PinnedRepository,
    name: &str,
    create: bool,
) -> Result<ReferenceParent, LocalGitFailure> {
    if name != "HEAD" && (!name.starts_with("refs/") || !git2::Reference::is_valid_name(name)) {
        return Err(LocalGitFailure::Operation);
    }
    let path = Path::new(name);
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .ok_or(LocalGitFailure::Operation)?
        .to_owned();
    let parent_path = path.parent().unwrap_or_else(|| Path::new(""));
    let creation_modes = if create && name.starts_with("refs/") {
        Some(reference_creation_modes(authority)?)
    } else {
        None
    };
    let mut directory = dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
    let mut relative = PathBuf::new();
    let mut hierarchy = vec![(
        relative.clone(),
        file_identity(
            &fs::File::from(dup(&directory).map_err(|_| LocalGitFailure::Operation)?)
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?,
        ),
    )];
    let mut created_directories = CreatedReferenceDirectories::default();
    for component in parent_path.components() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        let next_directory = if create {
            match creation_modes {
                Some((directory_mode, _)) => {
                    created_directories.open_or_create(&directory, component, directory_mode)?
                }
                None => open_or_create_ref_directory(&directory, component)?,
            }
        } else {
            openat(
                &directory,
                component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Operation)?
        };
        directory = next_directory;
        relative.push(component);
        let identity = file_identity(
            &fs::File::from(dup(&directory).map_err(|_| LocalGitFailure::Operation)?)
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?,
        );
        hierarchy.push((relative.clone(), identity));
    }
    Ok(ReferenceParent {
        directory,
        leaf,
        hierarchy,
        created_directories,
        creation_file_mode: creation_modes.map(|(_, file_mode)| file_mode),
    })
}

pub(super) fn reference_creation_modes(
    authority: &PinnedRepository,
) -> Result<(Mode, Mode), LocalGitFailure> {
    let refs = openat(
        &authority.git_directory,
        "refs",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    reference_installation_modes(&refs)
}

pub(super) fn reference_installation_modes(
    refs: &OwnedFd,
) -> Result<(Mode, Mode), LocalGitFailure> {
    let metadata = fs::File::from(dup(refs).map_err(|_| LocalGitFailure::Operation)?)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    let directory_mode = (metadata.mode() & 0o2777) | 0o700;
    let file_mode = (metadata.mode() & 0o666) | 0o600;
    Ok((
        Mode::from_raw_mode(directory_mode),
        Mode::from_raw_mode(file_mode),
    ))
}

pub(super) fn open_or_create_ref_directory(
    parent: &OwnedFd,
    name: &OsStr,
) -> Result<OwnedFd, LocalGitFailure> {
    open_or_create_ref_directory_with_mode(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)
}

pub(super) fn open_or_create_ref_directory_with_mode(
    parent: &OwnedFd,
    name: &OsStr,
    mode: Mode,
) -> Result<OwnedFd, LocalGitFailure> {
    open_or_create_ref_directory_with_mode_tracked(parent, name, mode)
        .map(|(directory, _)| directory)
}

pub(super) fn open_or_create_ref_directory_with_mode_tracked(
    parent: &OwnedFd,
    name: &OsStr,
    mode: Mode,
) -> Result<(OwnedFd, Option<CreatedReferenceDirectory>), LocalGitFailure> {
    open_or_create_ref_directory_with_mode_tracked_and_hook(parent, name, mode, || Ok(()))
}

pub(super) fn open_or_create_ref_directory_with_mode_tracked_and_hook<PostCreate>(
    parent: &OwnedFd,
    name: &OsStr,
    mode: Mode,
    post_create: PostCreate,
) -> Result<(OwnedFd, Option<CreatedReferenceDirectory>), LocalGitFailure>
where
    PostCreate: FnOnce() -> Result<(), LocalGitFailure>,
{
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(parent, name, flags, Mode::empty()) {
        Ok(directory) => Ok((directory, None)),
        Err(error) if error == rustix::io::Errno::NOENT => {
            let pinned_parent = dup(parent).map_err(|_| LocalGitFailure::Operation)?;
            mkdirat(parent, name, mode).map_err(|_| LocalGitFailure::Operation)?;
            let status = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(status) if FileType::from_raw_mode(status.st_mode) == FileType::Directory => {
                    status
                }
                Ok(_) | Err(_) => {
                    let _ = unlinkat(parent, name, AtFlags::REMOVEDIR);
                    return Err(LocalGitFailure::Operation);
                }
            };
            let created = CreatedReferenceDirectory {
                parent: pinned_parent,
                name: name.to_owned(),
                identity: FileIdentity {
                    device: status.st_dev,
                    inode: status.st_ino,
                },
            };
            if let Err(failure) = post_create() {
                created.remove_if_owned();
                return Err(failure);
            }
            let directory = match openat(parent, name, flags, Mode::empty()) {
                Ok(directory) => directory,
                Err(_) => {
                    created.remove_if_owned();
                    return Err(LocalGitFailure::Operation);
                }
            };
            let permission_file = match dup(&directory) {
                Ok(descriptor) => fs::File::from(descriptor),
                Err(_) => {
                    created.remove_if_owned();
                    return Err(LocalGitFailure::Operation);
                }
            };
            if permission_file
                .set_permissions(fs::Permissions::from_mode(mode.bits()))
                .is_err()
            {
                created.remove_if_owned();
                return Err(LocalGitFailure::Operation);
            }
            Ok((directory, Some(created)))
        }
        Err(_) => Err(LocalGitFailure::Operation),
    }
}
