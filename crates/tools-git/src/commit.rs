use std::{collections::HashSet, io::Read, path::Path};

use git2::{Odb, Repository, RepositoryState, Signature};

use crate::arguments::GitCommitArguments;
use crate::bounded::{
    find_bounded_commit, find_bounded_tree, validate_index_objects, validate_tree_discovery,
};
use crate::failure::LocalGitFailure;
use crate::identity::GitIdentity;
use crate::index_lock::IndexLock;
use crate::limits::{MAX_MERGE_HEAD_BYTES, MAX_MERGE_PARENTS};
use crate::objects::{PackRoot, persist_objects};
use crate::packed_reference::packed_reference_namespace_conflicts;
use crate::pinning::{PinnedRepository, pin_optional_git_file};
use crate::reference_lock::ReferenceLock;
use crate::reference_read::resolve_pinned_reference_chain;
use crate::reflog::ReferenceLogLock;
use crate::result::CommitResult;

pub(super) const COMMIT_REFLOG_ACTION: &str = "commit";

pub(super) fn read_merge_parent_ids(path: &Path) -> Result<Vec<git2::Oid>, LocalGitFailure> {
    let mut file =
        pin_optional_git_file(path, MAX_MERGE_HEAD_BYTES)?.ok_or(LocalGitFailure::Repository)?;
    let mut bytes = Vec::with_capacity(MAX_MERGE_HEAD_BYTES);
    Read::by_ref(&mut file)
        .take((MAX_MERGE_HEAD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Repository)?;
    if bytes.len() > MAX_MERGE_HEAD_BYTES {
        return Err(LocalGitFailure::Repository);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| LocalGitFailure::Repository)?;
    let mut parents = Vec::new();
    for line in text.lines() {
        if parents.len() == MAX_MERGE_PARENTS {
            return Err(LocalGitFailure::Repository);
        }
        parents.push(git2::Oid::from_str(line).map_err(|_| LocalGitFailure::Repository)?);
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
    let (_index_lock, mut index) = IndexLock::acquire_for_repository(authority)?;
    validate_index_objects(repository, &index)?;
    let state = repository.state();
    if !matches!(state, RepositoryState::Clean | RepositoryState::Merge) {
        return Err(LocalGitFailure::Operation);
    }
    let merge_parent_ids = if state == RepositoryState::Merge {
        read_merge_parent_ids(&repository.path().join("MERGE_HEAD"))?
    } else {
        Vec::new()
    };
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
    parent_ids.extend(merge_parent_ids);
    let mut unique_parent_ids = HashSet::new();
    parent_ids.retain(|oid| unique_parent_ids.insert(*oid));
    let parents = parent_ids
        .iter()
        .map(|oid| find_bounded_commit(repository, *oid))
        .collect::<Result<Vec<_>, _>>()?;
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
    let old = parent.unwrap_or(git2::Oid::ZERO_SHA1);
    validate_root_before_publish()?;
    publish_commit_reference(
        authority,
        update_lock,
        update_reference,
        old,
        oid,
        &signature,
    )?;
    let state_cleaned = state != RepositoryState::Merge || repository.cleanup_state().is_ok();
    Ok(CommitResult {
        commit: oid.to_string(),
        state_cleaned,
    })
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
