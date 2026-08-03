use std::{collections::HashSet, ffi::OsStr, fs, io::Read};

use git2::{ObjectFormat, Odb, Repository, Signature};
use rustix::fs::{AtFlags, Mode, OFlags, openat};
use rustix::io::dup;

use crate::arguments::GitCommitArguments;
use crate::bounded::{
    find_bounded_commit, find_bounded_tree, validate_index_objects, validate_tree_discovery,
};
use crate::descriptor::{
    FileSnapshotIdentity, descriptor_entry_exists, file_snapshot_identity, remove_entry_if_identity,
};
use crate::failure::LocalGitFailure;
use crate::identity::GitIdentity;
use crate::index_lock::IndexLock;
use crate::layout::parse_full_object_id_bytes;
use crate::limits::{
    MAX_COMMIT_MESSAGE_BYTES, MAX_MERGE_HEAD_BYTES, MAX_MERGE_PARENTS, MAX_REVISION_BYTES,
};
use crate::objects::{PackRoot, persist_objects};
use crate::packed_reference::packed_reference_namespace_conflicts;
use crate::pinning::PinnedRepository;
use crate::reference_lock::ReferenceLock;
use crate::reference_read::resolve_pinned_reference_chain;
use crate::reflog::ReferenceLogLock;
use crate::result::CommitResult;

pub(super) const COMMIT_REFLOG_ACTION: &str = "commit";

const OTHER_OPERATION_STATE_ENTRIES: [&str; 6] = [
    "rebase-merge",
    "rebase-apply",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "sequencer",
];

struct PinnedStateFile {
    name: &'static str,
    snapshot: FileSnapshotIdentity,
    bytes: Vec<u8>,
}

pub(super) struct RepositoryOperationState {
    merge_head: Option<PinnedStateFile>,
    merge_message: Option<PinnedStateFile>,
    merge_mode: Option<PinnedStateFile>,
}

impl RepositoryOperationState {
    pub(super) fn capture(authority: &PinnedRepository) -> Result<Self, LocalGitFailure> {
        reject_other_operation_states(authority)?;
        let merge_head = pin_state_file(authority, "MERGE_HEAD", MAX_MERGE_HEAD_BYTES)?;
        let merge_message = pin_state_file(authority, "MERGE_MSG", MAX_COMMIT_MESSAGE_BYTES)?;
        let merge_mode = pin_state_file(authority, "MERGE_MODE", MAX_REVISION_BYTES)?;
        if merge_head.is_none() && (merge_message.is_some() || merge_mode.is_some()) {
            return Err(LocalGitFailure::Repository);
        }
        let state = Self {
            merge_head,
            merge_message,
            merge_mode,
        };
        state.validate(authority)?;
        Ok(state)
    }

    pub(super) fn require_clean(&self) -> Result<(), LocalGitFailure> {
        if self.merge_head.is_none() {
            Ok(())
        } else {
            Err(LocalGitFailure::Operation)
        }
    }

    fn merge_parent_ids(
        &self,
        object_format: ObjectFormat,
    ) -> Result<Vec<git2::Oid>, LocalGitFailure> {
        self.merge_head.as_ref().map_or(Ok(Vec::new()), |head| {
            read_merge_parent_ids(&head.bytes, object_format)
        })
    }

    pub(super) fn validate(&self, authority: &PinnedRepository) -> Result<(), LocalGitFailure> {
        reject_other_operation_states(authority)?;
        validate_state_file(authority, "MERGE_HEAD", self.merge_head.as_ref())?;
        validate_state_file(authority, "MERGE_MSG", self.merge_message.as_ref())?;
        validate_state_file(authority, "MERGE_MODE", self.merge_mode.as_ref())
    }

    fn cleanup(&self, authority: &PinnedRepository) -> Result<(), LocalGitFailure> {
        self.validate(authority)?;
        let git_directory =
            dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
        for file in [
            self.merge_head.as_ref(),
            self.merge_message.as_ref(),
            self.merge_mode.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            remove_entry_if_identity(
                &git_directory,
                OsStr::new(file.name),
                file.snapshot.file,
                AtFlags::empty(),
            )?;
        }
        Ok(())
    }
}

fn pin_state_file(
    authority: &PinnedRepository,
    name: &'static str,
    max_bytes: usize,
) -> Result<Option<PinnedStateFile>, LocalGitFailure> {
    let descriptor = match openat(
        &authority.git_directory,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(_) => return Err(LocalGitFailure::Repository),
    };
    let mut file = fs::File::from(descriptor);
    let before = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
    if !before.is_file() || before.len() > max_bytes as u64 {
        return Err(LocalGitFailure::Repository);
    }
    let mut bytes = Vec::with_capacity(max_bytes.min(before.len() as usize));
    Read::by_ref(&mut file)
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Repository)?;
    let after = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
    if bytes.len() > max_bytes
        || file_snapshot_identity(&before) != file_snapshot_identity(&after)
        || bytes.len() as u64 != before.len()
    {
        return Err(LocalGitFailure::Repository);
    }
    Ok(Some(PinnedStateFile {
        name,
        snapshot: file_snapshot_identity(&after),
        bytes,
    }))
}

fn validate_state_file(
    authority: &PinnedRepository,
    name: &'static str,
    expected: Option<&PinnedStateFile>,
) -> Result<(), LocalGitFailure> {
    let max_bytes = match name {
        "MERGE_HEAD" => MAX_MERGE_HEAD_BYTES,
        "MERGE_MSG" => MAX_COMMIT_MESSAGE_BYTES,
        "MERGE_MODE" => MAX_REVISION_BYTES,
        _ => return Err(LocalGitFailure::Repository),
    };
    let current = pin_state_file(authority, name, max_bytes)?;
    match (expected, current) {
        (None, None) => Ok(()),
        (Some(expected), Some(current))
            if expected.snapshot == current.snapshot && expected.bytes == current.bytes =>
        {
            Ok(())
        }
        (None, Some(_)) | (Some(_), None) | (Some(_), Some(_)) => Err(LocalGitFailure::Repository),
    }
}

fn reject_other_operation_states(authority: &PinnedRepository) -> Result<(), LocalGitFailure> {
    let git_directory = dup(&authority.git_directory).map_err(|_| LocalGitFailure::Repository)?;
    for name in OTHER_OPERATION_STATE_ENTRIES {
        if descriptor_entry_exists(&git_directory, OsStr::new(name))? {
            return Err(LocalGitFailure::Operation);
        }
    }
    Ok(())
}

pub(super) fn read_merge_parent_ids(
    bytes: &[u8],
    object_format: ObjectFormat,
) -> Result<Vec<git2::Oid>, LocalGitFailure> {
    let records = bytes
        .strip_suffix(b"\n")
        .ok_or(LocalGitFailure::Repository)?;
    let mut parents = Vec::new();
    for line in records.split(|byte| *byte == b'\n') {
        if parents.len() == MAX_MERGE_PARENTS {
            return Err(LocalGitFailure::Repository);
        }
        parents.push(
            parse_full_object_id_bytes(line, object_format).ok_or(LocalGitFailure::Repository)?,
        );
    }
    if parents.is_empty() {
        Err(LocalGitFailure::Repository)
    } else {
        Ok(parents)
    }
}

pub(super) fn commit<ValidateRoot>(
    repository: &mut Repository,
    identity: &GitIdentity,
    arguments: GitCommitArguments,
    authority: &PinnedRepository,
    persistent_object_database: &Odb<'_>,
    object_database: &Odb<'_>,
    validate_root_before_publish: ValidateRoot,
) -> Result<CommitResult, LocalGitFailure>
where
    ValidateRoot: FnOnce() -> Result<(), LocalGitFailure>,
{
    let (index_lock, mut index) = IndexLock::acquire_for_repository(authority)?;
    validate_index_objects(repository, &index)?;
    let state = RepositoryOperationState::capture(authority)?;
    let merge_parent_ids = state.merge_parent_ids(authority.object_format)?;
    let (reference_chain, initial_parent) = resolve_pinned_reference_chain(authority, None)?;
    let update_reference = reference_chain.last().ok_or(LocalGitFailure::Operation)?;
    if packed_reference_namespace_conflicts(authority, update_reference)? {
        return Err(LocalGitFailure::Operation);
    }
    let mut reference_locks = reference_chain
        .iter()
        .map(|reference| ReferenceLock::acquire(authority, reference))
        .collect::<Result<Vec<_>, _>>()?;
    let (locked_chain, parent) = resolve_pinned_reference_chain(authority, Some(&reference_locks))?;
    if locked_chain != reference_chain || parent != initial_parent {
        return Err(LocalGitFailure::Operation);
    }
    let mut parent_ids = parent.into_iter().collect::<Vec<_>>();
    let is_ordinary_commit = merge_parent_ids.is_empty();
    parent_ids.extend(merge_parent_ids);
    let mut unique_parent_ids = HashSet::new();
    parent_ids.retain(|oid| unique_parent_ids.insert(*oid));
    let parents = parent_ids
        .iter()
        .map(|oid| find_bounded_commit(repository, *oid))
        .collect::<Result<Vec<_>, _>>()?;
    if is_ordinary_commit && parents.len() == 1 {
        let parent_tree = find_bounded_tree(repository, parents[0].tree_id())?;
        let changes = repository
            .diff_tree_to_index(Some(&parent_tree), Some(&index), None)
            .map_err(|_| LocalGitFailure::Operation)?;
        if changes.deltas().len() == 0 {
            return Err(LocalGitFailure::Operation);
        }
    }
    let tree_id = index
        .write_tree_to(repository)
        .map_err(|_| LocalGitFailure::Operation)?;
    let tree = find_bounded_tree(repository, tree_id)?;
    validate_tree_discovery(repository, &tree)?;
    let signature = identity
        .signature()
        .map_err(|_| LocalGitFailure::Operation)?;
    let parent_refs = parents.iter().collect::<Vec<_>>();
    let oid = repository
        .commit(
            None,
            &signature,
            &signature,
            &arguments.message,
            &tree,
            &parent_refs,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
    persist_objects(
        authority,
        repository,
        persistent_object_database,
        object_database,
        &[PackRoot::Commit(oid)],
    )?;
    let update_reference = locked_chain.last().ok_or(LocalGitFailure::Operation)?;
    if reference_locks
        .iter()
        .any(|lock| !lock.hierarchy_is_current(authority))
    {
        return Err(LocalGitFailure::Operation);
    }
    let (current_chain, current_parent) =
        resolve_pinned_reference_chain(authority, Some(&reference_locks))?;
    if current_chain != locked_chain || current_parent != parent {
        return Err(LocalGitFailure::Operation);
    }
    if packed_reference_namespace_conflicts(authority, update_reference)? {
        return Err(LocalGitFailure::Operation);
    }
    let update_lock = reference_locks
        .iter()
        .position(|lock| lock.name == *update_reference)
        .map(|position| reference_locks.swap_remove(position))
        .ok_or(LocalGitFailure::Operation)?;
    let old = parent.unwrap_or(match authority.object_format {
        ObjectFormat::Sha1 => git2::Oid::ZERO_SHA1,
        ObjectFormat::Sha256 => git2::Oid::ZERO_SHA256,
    });
    state.validate(authority)?;
    validate_root_before_publish()?;
    index_lock.validate_source()?;
    validate_live_commit_target(authority, oid)?;
    publish_commit_reference(
        authority,
        update_lock,
        update_reference,
        old,
        oid,
        &signature,
    )?;
    let state_cleaned = state.cleanup(authority).is_ok();
    Ok(CommitResult {
        commit: oid.to_string(),
        state_cleaned,
    })
}

fn validate_live_commit_target(
    authority: &PinnedRepository,
    target: git2::Oid,
) -> Result<(), LocalGitFailure> {
    let repository = authority.open_repository_shell()?;
    let pinned_objects = crate::pinning::PinnedObjectDatabase::capture(authority)?;
    let object_database =
        Odb::new_ext(authority.object_format).map_err(|_| LocalGitFailure::Operation)?;
    pinned_objects.add_to(&object_database)?;
    repository
        .set_odb(&object_database)
        .map_err(|_| LocalGitFailure::Operation)?;
    let commit = find_bounded_commit(&repository, target)?;
    let tree = find_bounded_tree(&repository, commit.tree_id())?;
    validate_tree_discovery(&repository, &tree)?;
    pinned_objects.validate_live(authority)
}

pub(super) fn publish_commit_reference(
    authority: &PinnedRepository,
    update_lock: ReferenceLock,
    update_reference: &str,
    old: git2::Oid,
    new: git2::Oid,
    signature: &Signature<'_>,
) -> Result<(), LocalGitFailure> {
    publish_commit_reference_with_hook(
        authority,
        update_lock,
        update_reference,
        old,
        new,
        signature,
        || {},
    )
}

pub(super) fn publish_commit_reference_with_hook<Hook: FnOnce()>(
    authority: &PinnedRepository,
    mut update_lock: ReferenceLock,
    update_reference: &str,
    old: git2::Oid,
    new: git2::Oid,
    signature: &Signature<'_>,
    before_reference_publish: Hook,
) -> Result<(), LocalGitFailure> {
    let expected = update_lock.read(authority)?;
    update_lock.prepare(authority, new)?;
    let mut logs = vec![ReferenceLogLock::acquire(authority, "HEAD")?];
    if update_reference != "HEAD" {
        logs.push(ReferenceLogLock::acquire(authority, update_reference)?);
    }
    for log in &mut logs {
        log.append(old, new, signature, COMMIT_REFLOG_ACTION)?;
    }
    let mut published = 0_usize;
    while published < logs.len() {
        if logs[published].publish().is_err() {
            rollback_published_logs(&mut logs[..published]);
            return Err(LocalGitFailure::Operation);
        }
        published += 1;
    }
    before_reference_publish();
    if update_lock.publish(authority, &expected).is_err() {
        rollback_published_logs(&mut logs);
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

pub(super) fn publish_symbolic_head<ValidateRoot>(
    authority: &PinnedRepository,
    mut head_lock: ReferenceLock,
    target_reference: &str,
    old: git2::Oid,
    new: git2::Oid,
    signature: &Signature<'_>,
    validate_root_before_publish: ValidateRoot,
) -> Result<(), LocalGitFailure>
where
    ValidateRoot: FnOnce() -> Result<(), LocalGitFailure>,
{
    let expected = head_lock.read(authority)?;
    head_lock.prepare_symbolic(authority, target_reference)?;
    let mut log = ReferenceLogLock::acquire(authority, "HEAD")?;
    log.append(
        old,
        new,
        signature,
        "checkout: moving to configured local branch",
    )?;
    log.publish()?;
    if let Err(failure) = validate_root_before_publish() {
        let _ = log.rollback();
        return Err(failure);
    }
    if head_lock.publish(authority, &expected).is_err() {
        let _ = log.rollback();
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

pub(super) fn rollback_published_logs(logs: &mut [ReferenceLogLock]) {
    for log in logs.iter_mut().rev() {
        let _ = log.rollback();
    }
}
