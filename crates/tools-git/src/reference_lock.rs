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
        AtFlags, FileType, Mode, OFlags, RenameFlags, fchmod, mkdirat, openat, renameat_with,
        statat, unlinkat,
    },
    io::dup,
};

use crate::descriptor::{
    FileIdentity, QuarantineDirectory, descriptor_entry_exists, file_identity,
    remove_entry_if_identity, stat_file_identity,
};
use crate::failure::LocalGitFailure;
use crate::layout::valid_reference_name;
use crate::limits::{MAX_REFERENCE_BYTES, MAX_REVISION_BYTES};
use crate::packed_reference::{PackedReferenceNamespace, packed_reference_state};
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
    creation_file_mode: Option<Mode>,
}

#[derive(Clone, Copy)]
pub(super) enum ReferenceParentMode {
    CreateMissing,
    ExistingOnly,
}

struct ReferencePublicationHooks<
    BeforeAbsent,
    BeforeExchange,
    AfterPublish,
    BeforeCleanup,
    AfterDisplacedQuarantine,
    BeforeRollback,
> {
    before_absent: BeforeAbsent,
    before_exchange: BeforeExchange,
    after_publish: AfterPublish,
    before_cleanup: BeforeCleanup,
    after_displaced_quarantine: AfterDisplacedQuarantine,
    before_rollback: BeforeRollback,
}

impl ReferenceLock {
    pub(super) fn acquire(
        authority: &PinnedRepository,
        name: &str,
    ) -> Result<Self, LocalGitFailure> {
        authority.validate_supported_layout()?;
        let bound = open_reference_parent(authority, name, ReferenceParentMode::CreateMissing)?;
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
        authority.validate_supported_layout()?;
        Ok(guard)
    }

    pub(super) fn read(
        &self,
        authority: &PinnedRepository,
    ) -> Result<PinnedReferenceValue, LocalGitFailure> {
        let value = read_reference_leaf(&self.parent, &self.leaf, authority, &self.name)?;
        if !self.hierarchy_is_current(authority) {
            return Err(LocalGitFailure::Operation);
        }
        Ok(value)
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
        self.prepare_with_hook(authority, target, || {})
    }

    #[cfg(test)]
    pub(super) fn prepare_with_test_hook<AfterWrite: FnOnce()>(
        &mut self,
        authority: &PinnedRepository,
        target: git2::Oid,
        after_write: AfterWrite,
    ) -> Result<(), LocalGitFailure> {
        self.prepare_with_hook(authority, target, after_write)
    }

    fn prepare_with_hook<AfterWrite: FnOnce()>(
        &mut self,
        authority: &PinnedRepository,
        target: git2::Oid,
        after_write: AfterWrite,
    ) -> Result<(), LocalGitFailure> {
        authority.validate_supported_layout()?;
        if target.object_format() != authority.object_format {
            return Err(LocalGitFailure::Operation);
        }
        let expected = format!("{target}\n");
        self.lock
            .set_len(0)
            .and_then(|()| self.lock.rewind())
            .and_then(|()| self.lock.write_all(expected.as_bytes()))
            .map_err(|_| LocalGitFailure::Operation)?;
        self.lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        after_write();
        self.record_prepared_reference(expected.as_bytes())?;
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
        authority.validate_supported_layout()?;
        if !target.starts_with("refs/") || validate_reference_name(target).is_err() {
            return Err(LocalGitFailure::Operation);
        }
        let expected = format!("ref: {target}\n");
        self.lock
            .set_len(0)
            .and_then(|()| self.lock.rewind())
            .and_then(|()| self.lock.write_all(expected.as_bytes()))
            .map_err(|_| LocalGitFailure::Operation)?;
        self.lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        self.record_prepared_reference(expected.as_bytes())?;
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
            ReferencePublicationHooks {
                before_absent: before_absent_publish,
                before_exchange: || {},
                after_publish: || {},
                before_cleanup: || {},
                after_displaced_quarantine: || {},
                before_rollback: || {},
            },
        )
    }

    fn publish_with_hooks<
        BeforeAbsent: FnOnce(),
        BeforeExchange: FnOnce(),
        AfterPublish: FnOnce(),
        BeforeCleanup: FnOnce(),
        AfterDisplacedQuarantine: FnOnce(),
        BeforeRollback: FnOnce(),
    >(
        mut self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        hooks: ReferencePublicationHooks<
            BeforeAbsent,
            BeforeExchange,
            AfterPublish,
            BeforeCleanup,
            AfterDisplacedQuarantine,
            BeforeRollback,
        >,
    ) -> Result<(), LocalGitFailure> {
        let ReferencePublicationHooks {
            before_absent,
            before_exchange,
            after_publish,
            before_cleanup,
            after_displaced_quarantine,
            before_rollback,
        } = hooks;
        let mut before_rollback = Some(before_rollback);
        authority.validate_supported_layout()?;
        if !self.path_still_owned()
            || !self.prepared_lock_is_current()
            || !self.hierarchy_is_current(authority)
        {
            return Err(LocalGitFailure::Operation);
        }
        let expected_packed = packed_reference_state(authority, &self.name)?;
        if expected_packed.namespace == PackedReferenceNamespace::Conflicts {
            return Err(LocalGitFailure::Operation);
        }
        if !descriptor_entry_exists(&self.parent, &self.leaf)? {
            if self.read(authority)? != *expected {
                return Err(LocalGitFailure::Operation);
            }
            before_absent();
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
            let packed_state_is_current = packed_reference_state(authority, &self.name)
                .is_ok_and(|current| current == expected_packed);
            let layout_is_current = authority.validate_supported_layout().is_ok();
            let hierarchy_is_current = self.hierarchy_is_current(authority);
            before_cleanup();
            let publication_is_current = self.prepared_publication_is_current();
            if !packed_state_is_current
                || !layout_is_current
                || !hierarchy_is_current
                || !publication_is_current
            {
                before_rollback.take().ok_or(LocalGitFailure::Operation)?();
                if publication_is_current {
                    let prepared = self.prepared.ok_or(LocalGitFailure::Operation)?;
                    let _ =
                        remove_published_reference_if_current(&self.parent, &self.leaf, prepared);
                }
                return Err(LocalGitFailure::Operation);
            }
            self.committed = true;
            return Ok(());
        }
        let expected_leaf_snapshot = reference_snapshot_identity_at(&self.parent, &self.leaf)?
            .ok_or(LocalGitFailure::Operation)?;
        before_exchange();
        if reference_snapshot_identity_at(&self.parent, &self.leaf)? != Some(expected_leaf_snapshot)
        {
            return Err(LocalGitFailure::Operation);
        }
        renameat_with(
            &self.parent,
            &self.lock_name,
            &self.parent,
            &self.leaf,
            RenameFlags::EXCHANGE,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        after_publish();
        let displaced_after_exchange =
            match reference_snapshot_identity_at(&self.parent, &self.lock_name) {
                Ok(Some(displaced)) if displaced == expected_leaf_snapshot => displaced,
                Ok(Some(_)) | Ok(None) | Err(_) => {
                    let publication_is_current = self.prepared_publication_is_current();
                    let packed_state_is_current = packed_reference_state(authority, &self.name)
                        .is_ok_and(|current| current == expected_packed);
                    if publication_is_current
                        && packed_state_is_current
                        && authority.validate_supported_layout().is_ok()
                        && self.hierarchy_is_current(authority)
                    {
                        self.committed = true;
                        return Ok(());
                    }
                    return Err(LocalGitFailure::Operation);
                }
            };
        let displaced = read_reference_leaf(&self.parent, &self.lock_name, authority, &self.name);
        let packed_state_is_current = packed_reference_state(authority, &self.name)
            .is_ok_and(|current| current == expected_packed);
        let displaced_value_is_expected = displaced.as_ref() == Ok(expected);
        let displaced_snapshot_is_current =
            reference_snapshot_identity_at(&self.parent, &self.lock_name)
                == Ok(Some(expected_leaf_snapshot));
        let publication_is_current = self.prepared_publication_is_current();
        let layout_is_current = authority.validate_supported_layout().is_ok();
        if !displaced_value_is_expected
            || !displaced_snapshot_is_current
            || !publication_is_current
            || !packed_state_is_current
            || !layout_is_current
            || !self.hierarchy_is_current(authority)
        {
            before_rollback.take().ok_or(LocalGitFailure::Operation)?();
            if publication_is_current {
                let _ = rollback_reference_exchange_if_current(
                    &self.parent,
                    &self.leaf,
                    &self.lock_name,
                    displaced_after_exchange,
                    self.prepared.ok_or(LocalGitFailure::Operation)?,
                );
            }
            return Err(LocalGitFailure::Operation);
        }
        before_cleanup();
        let final_postconditions_hold = packed_reference_state(authority, &self.name)
            .is_ok_and(|current| current == expected_packed)
            && authority.validate_supported_layout().is_ok()
            && self.hierarchy_is_current(authority);
        if !final_postconditions_hold {
            let _ = rollback_reference_exchange_if_current(
                &self.parent,
                &self.leaf,
                &self.lock_name,
                displaced_after_exchange,
                self.prepared.ok_or(LocalGitFailure::Operation)?,
            );
            return Err(LocalGitFailure::Operation);
        }
        finalize_reference_exchange_if_current(
            &self.parent,
            &self.leaf,
            &self.lock_name,
            expected_leaf_snapshot,
            self.prepared.ok_or(LocalGitFailure::Operation)?,
            after_displaced_quarantine,
        )?;
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
            ReferencePublicationHooks {
                before_absent: before_absent_publish,
                before_exchange: || {},
                after_publish,
                before_cleanup: || {},
                after_displaced_quarantine: || {},
                before_rollback: || {},
            },
        )
    }

    #[cfg(test)]
    pub(super) fn publish_with_cleanup_test_hook<BeforeCleanup: FnOnce()>(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        before_cleanup: BeforeCleanup,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hooks(
            authority,
            expected,
            ReferencePublicationHooks {
                before_absent: || {},
                before_exchange: || {},
                after_publish: || {},
                before_cleanup,
                after_displaced_quarantine: || {},
                before_rollback: || {},
            },
        )
    }

    #[cfg(test)]
    pub(super) fn publish_with_finalization_test_hook<AfterDisplacedQuarantine: FnOnce()>(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        after_displaced_quarantine: AfterDisplacedQuarantine,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hooks(
            authority,
            expected,
            ReferencePublicationHooks {
                before_absent: || {},
                before_exchange: || {},
                after_publish: || {},
                before_cleanup: || {},
                after_displaced_quarantine,
                before_rollback: || {},
            },
        )
    }

    #[cfg(test)]
    pub(super) fn publish_with_pre_exchange_test_hook<BeforeExchange: FnOnce()>(
        self,
        authority: &PinnedRepository,
        expected: &PinnedReferenceValue,
        before_exchange: BeforeExchange,
    ) -> Result<(), LocalGitFailure> {
        self.publish_with_hooks(
            authority,
            expected,
            ReferencePublicationHooks {
                before_absent: || {},
                before_exchange,
                after_publish: || {},
                before_cleanup: || {},
                after_displaced_quarantine: || {},
                before_rollback: || {},
            },
        )
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
            ReferencePublicationHooks {
                before_absent: || {},
                before_exchange: || {},
                after_publish,
                before_cleanup: || {},
                after_displaced_quarantine: || {},
                before_rollback,
            },
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

    fn record_prepared_reference(&mut self, expected: &[u8]) -> Result<(), LocalGitFailure> {
        let snapshot = reference_snapshot_identity(&mut self.lock)?;
        let expected_length =
            u64::try_from(expected.len()).map_err(|_| LocalGitFailure::Operation)?;
        let expected_digest: [u8; 32] = Sha256::digest(expected).into();
        if snapshot.file != self.identity
            || snapshot.length != expected_length
            || snapshot.digest != expected_digest
        {
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

impl ReferenceParent {
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
}

fn finalize_reference_exchange_if_current(
    parent: &OwnedFd,
    leaf: &OsStr,
    lock_name: &OsStr,
    displaced: ReferenceSnapshotIdentity,
    publication: ReferenceSnapshotIdentity,
    after_displaced_quarantine: impl FnOnce(),
) -> Result<(), LocalGitFailure> {
    if reference_snapshot_identity_at(parent, leaf) != Ok(Some(publication)) {
        return Err(LocalGitFailure::Operation);
    }
    let quarantine = QuarantineDirectory::create(parent)?;
    let quarantined_displaced = OsStr::new("displaced");
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
    after_displaced_quarantine();
    if reference_snapshot_identity_at(parent, leaf) != Ok(Some(publication)) {
        restore_or_remove_quarantined_reference(
            &quarantine,
            quarantined_displaced,
            parent,
            lock_name,
            Some(displaced),
        )?;
        return Err(LocalGitFailure::Operation);
    }
    if unlinkat(
        quarantine.descriptor(),
        quarantined_displaced,
        AtFlags::empty(),
    )
    .is_err()
    {
        restore_or_remove_quarantined_reference(
            &quarantine,
            quarantined_displaced,
            parent,
            lock_name,
            Some(displaced),
        )?;
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

fn remove_published_reference_if_current(
    parent: &OwnedFd,
    name: &OsStr,
    expected: ReferenceSnapshotIdentity,
) -> Result<(), LocalGitFailure> {
    let quarantine = QuarantineDirectory::create(parent)?;
    let quarantined_name = OsStr::new("publication");
    renameat_with(
        parent,
        name,
        quarantine.descriptor(),
        quarantined_name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    if reference_snapshot_identity_at(quarantine.descriptor(), quarantined_name)
        != Ok(Some(expected))
    {
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

fn restore_quarantined_publication(
    parent: &OwnedFd,
    leaf: &OsStr,
    lock_name: &OsStr,
    quarantine: &QuarantineDirectory,
    quarantined_displaced: &OsStr,
    quarantined_publication: &OsStr,
    publication: Option<ReferenceSnapshotIdentity>,
) -> Result<(), LocalGitFailure> {
    restore_or_remove_quarantined_reference(
        quarantine,
        quarantined_displaced,
        parent,
        lock_name,
        None,
    )?;
    restore_or_remove_quarantined_reference(
        quarantine,
        quarantined_publication,
        parent,
        leaf,
        publication,
    )
}

fn restore_or_remove_quarantined_reference(
    quarantine: &QuarantineDirectory,
    quarantined_name: &OsStr,
    parent: &OwnedFd,
    name: &OsStr,
    expected: Option<ReferenceSnapshotIdentity>,
) -> Result<(), LocalGitFailure> {
    if renameat_with(
        quarantine.descriptor(),
        quarantined_name,
        parent,
        name,
        RenameFlags::NOREPLACE,
    )
    .is_ok()
    {
        return Ok(());
    }
    let expected = expected.ok_or(LocalGitFailure::Operation)?;
    if reference_snapshot_identity_at(quarantine.descriptor(), quarantined_name)
        != Ok(Some(expected))
    {
        return Err(LocalGitFailure::Operation);
    }
    unlinkat(quarantine.descriptor(), quarantined_name, AtFlags::empty())
        .map_err(|_| LocalGitFailure::Operation)
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
        restore_quarantined_publication(
            parent,
            leaf,
            lock_name,
            &quarantine,
            quarantined_displaced,
            quarantined_publication,
            None,
        )?;
        return Err(LocalGitFailure::Operation);
    }
    if renameat_with(
        quarantine.descriptor(),
        quarantined_displaced,
        parent,
        leaf,
        RenameFlags::NOREPLACE,
    )
    .is_err()
    {
        restore_quarantined_publication(
            parent,
            leaf,
            lock_name,
            &quarantine,
            quarantined_displaced,
            quarantined_publication,
            Some(publication),
        )?;
        return Err(LocalGitFailure::Operation);
    }
    if renameat_with(
        quarantine.descriptor(),
        quarantined_publication,
        parent,
        lock_name,
        RenameFlags::NOREPLACE,
    )
    .is_err()
    {
        restore_or_remove_quarantined_reference(
            &quarantine,
            quarantined_publication,
            parent,
            lock_name,
            Some(publication),
        )?;
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
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

pub(super) fn open_reference_parent(
    authority: &PinnedRepository,
    name: &str,
    mode: ReferenceParentMode,
) -> Result<ReferenceParent, LocalGitFailure> {
    validate_reference_name(name)?;
    let path = Path::new(name);
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .ok_or(LocalGitFailure::Operation)?
        .to_owned();
    let parent_path = path.parent().unwrap_or_else(|| Path::new(""));
    let creation_modes = match mode {
        ReferenceParentMode::CreateMissing if name.starts_with("refs/") => {
            Some(reference_creation_modes(authority)?)
        }
        ReferenceParentMode::CreateMissing | ReferenceParentMode::ExistingOnly => None,
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
    for component in parent_path.components() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        let next_directory = match mode {
            ReferenceParentMode::CreateMissing => match creation_modes {
                Some(creation_modes) => open_or_create_ref_directory_with_mode_tracked(
                    &directory,
                    component,
                    creation_modes.directory,
                )?,
                None => open_or_create_ref_directory(&directory, component)?,
            },
            ReferenceParentMode::ExistingOnly => openat(
                &directory,
                component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Operation)?,
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
        creation_file_mode: creation_modes.map(|modes| modes.file),
    })
}

pub(super) fn validate_reference_name(name: &str) -> Result<(), LocalGitFailure> {
    if name.len() > MAX_REFERENCE_BYTES
        || (name != "HEAD"
            && (!name.starts_with("refs/") || !valid_reference_name(name.as_bytes())))
    {
        Err(LocalGitFailure::Operation)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) struct ReferenceInstallationModes {
    pub(super) directory: Mode,
    pub(super) file: Mode,
}

pub(super) fn reference_creation_modes(
    authority: &PinnedRepository,
) -> Result<ReferenceInstallationModes, LocalGitFailure> {
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
) -> Result<ReferenceInstallationModes, LocalGitFailure> {
    let metadata = fs::File::from(dup(refs).map_err(|_| LocalGitFailure::Operation)?)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    let directory_mode = (metadata.mode() & 0o2777) | 0o700;
    let file_mode = (metadata.mode() & 0o666) | 0o600;
    Ok(ReferenceInstallationModes {
        directory: Mode::from_raw_mode(directory_mode),
        file: Mode::from_raw_mode(file_mode),
    })
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
}

pub(super) fn open_or_create_ref_directory_with_mode_tracked(
    parent: &OwnedFd,
    name: &OsStr,
    mode: Mode,
) -> Result<OwnedFd, LocalGitFailure> {
    open_or_create_ref_directory_with_mode_tracked_and_hook(parent, name, mode, || Ok(()))
}

pub(super) fn open_or_create_ref_directory_with_mode_tracked_and_hook<PostCreate>(
    parent: &OwnedFd,
    name: &OsStr,
    mode: Mode,
    post_create: PostCreate,
) -> Result<OwnedFd, LocalGitFailure>
where
    PostCreate: FnOnce() -> Result<(), LocalGitFailure>,
{
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(parent, name, flags, Mode::empty()) {
        Ok(directory) => Ok(directory),
        Err(error) if error == rustix::io::Errno::NOENT => {
            mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)
                .map_err(|_| LocalGitFailure::Operation)?;
            let created = openat(parent, name, flags, Mode::empty())
                .map_err(|_| LocalGitFailure::Operation)?;
            let created_identity = file_identity(
                &fs::File::from(dup(&created).map_err(|_| LocalGitFailure::Operation)?)
                    .metadata()
                    .map_err(|_| LocalGitFailure::Operation)?,
            );
            post_create()?;
            let current = match openat(parent, name, flags, Mode::empty()) {
                Ok(directory) => directory,
                Err(_) => return Err(LocalGitFailure::Operation),
            };
            let current_identity = file_identity(
                &fs::File::from(dup(&current).map_err(|_| LocalGitFailure::Operation)?)
                    .metadata()
                    .map_err(|_| LocalGitFailure::Operation)?,
            );
            if current_identity != created_identity {
                return Ok(current);
            }
            fchmod(&created, mode).map_err(|_| LocalGitFailure::Operation)?;
            let created_metadata =
                fs::File::from(dup(&created).map_err(|_| LocalGitFailure::Operation)?)
                    .metadata()
                    .map_err(|_| LocalGitFailure::Operation)?;
            if file_identity(&created_metadata) != created_identity
                || created_metadata.mode() & 0o2777 != mode.bits() & 0o2777
            {
                return Err(LocalGitFailure::Operation);
            }
            let path_identity = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                .ok()
                .filter(|status| FileType::from_raw_mode(status.st_mode) == FileType::Directory)
                .map(|status| stat_file_identity(&status));
            if path_identity != Some(created_identity) {
                return Err(LocalGitFailure::Operation);
            }
            Ok(created)
        }
        Err(_) => Err(LocalGitFailure::Operation),
    }
}
