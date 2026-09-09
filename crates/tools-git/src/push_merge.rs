//! Merge-forward hunk preservation before push; see tool-loop.md.

use std::{collections::HashSet, time::Instant};

use git2::{Diff, DiffOptions, ObjectType, Oid, Patch};
use serde::Serialize;

use crate::{
    limits::MAX_REPOSITORY_INSPECTIONS, pinning::PinnedRepository, push_executor::GitPushFailure,
    push_objects::ObjectSource,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct DroppedBaseChanges {
    pub(super) file: String,
    pub(super) first_dropped_hunk: String,
}

fn repository_failure<T>(_: T) -> GitPushFailure {
    GitPushFailure::Repository
}

pub(super) fn verify_merge(
    authority: &PinnedRepository,
    target: Oid,
    deadline: Instant,
) -> Result<(), GitPushFailure> {
    // A separate shell keeps the push snapshot's shallow dispatch fence intact.
    let repository = authority
        .open_repository_shell()
        .map_err(repository_failure)?;
    let database = repository.odb().map_err(repository_failure)?;
    let mut source = ObjectSource::open(authority, deadline).map_err(repository_failure)?;
    source
        .capture(&database, target)
        .map_err(repository_failure)?;
    let merge = repository.find_commit(target).map_err(repository_failure)?;
    if merge.parent_count() < 2 {
        return Ok(());
    }
    let branch = merge.parent_id(0).map_err(repository_failure)?;
    let mut commits: Vec<_> = merge.parent_ids().collect();
    let mut visited = HashSet::new();
    while let Some(oid) = commits.pop() {
        if !visited.insert(oid) {
            continue;
        }
        if visited.len() > MAX_REPOSITORY_INSPECTIONS {
            return Err(GitPushFailure::Repository);
        }
        source.capture(&database, oid).map_err(repository_failure)?;
        commits.extend(
            repository
                .find_commit(oid)
                .map_err(repository_failure)?
                .parent_ids(),
        );
    }
    let mut dropped = Vec::new();
    for base in merge.parent_ids().skip(1) {
        let ancestor = repository
            .merge_base(branch, base)
            .map_err(repository_failure)?;
        let mut trees = Vec::new();
        for commit in [target, branch, base, ancestor] {
            trees.push(
                repository
                    .find_commit(commit)
                    .map_err(repository_failure)?
                    .tree_id(),
            );
        }
        let mut visited = HashSet::new();
        while let Some(oid) = trees.pop() {
            if !visited.insert(oid) {
                continue;
            }
            if visited.len() > MAX_REPOSITORY_INSPECTIONS {
                return Err(GitPushFailure::Repository);
            }
            source.capture(&database, oid).map_err(repository_failure)?;
            for entry in &repository.find_tree(oid).map_err(repository_failure)? {
                if entry.kind() == Some(ObjectType::Tree) {
                    trees.push(entry.id());
                }
            }
        }
        let tree = |oid| repository.find_commit(oid)?.tree();
        let merge_tree = tree(target).map_err(repository_failure)?;
        let base_tree = tree(base).map_err(repository_failure)?;
        let branch_tree = tree(branch).map_err(repository_failure)?;
        let ancestor_tree = tree(ancestor).map_err(repository_failure)?;
        let mut options = diff_options();
        let carried = repository
            .diff_tree_to_tree(Some(&base_tree), Some(&merge_tree), Some(&mut options))
            .map_err(repository_failure)?;
        for (index, delta) in carried.deltas().enumerate() {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .ok_or(GitPushFailure::Repository)?;
            let mut options = diff_options();
            options.disable_pathspec_match(true).pathspec(path);
            let own = repository
                .diff_tree_to_tree(Some(&ancestor_tree), Some(&branch_tree), Some(&mut options))
                .map_err(repository_failure)?;
            // Diff only touched paths; unchanged fence blobs need not be copied.
            for delta in std::iter::once(delta).chain(own.deltas()) {
                for file in [delta.old_file(), delta.new_file()] {
                    if !file.id().is_zero() && file.mode() != git2::FileMode::Commit {
                        source
                            .capture(&database, file.id())
                            .map_err(repository_failure)?;
                    }
                }
            }
            let mut permitted = Vec::new();
            for index in 0..own.deltas().len() {
                permitted.extend(hunks(&own, index)?);
            }
            for hunk in hunks(&carried, index)? {
                if let Some(index) = permitted.iter().position(|own| own == &hunk) {
                    permitted.remove(index);
                } else {
                    dropped.push(DroppedBaseChanges {
                        file: path.to_string_lossy().into_owned(),
                        first_dropped_hunk: String::from_utf8_lossy(&hunk).into_owned(),
                    });
                    break;
                }
            }
        }
    }
    source.validate(authority).map_err(repository_failure)?;
    if dropped.is_empty() {
        Ok(())
    } else {
        dropped.sort_by(|a, b| a.file.cmp(&b.file));
        dropped.dedup_by(|a, b| a.file == b.file);
        Err(GitPushFailure::MergeDroppedBaseChanges(dropped))
    }
}

fn diff_options() -> DiffOptions {
    let mut options = DiffOptions::new();
    options
        .context_lines(0)
        .interhunk_lines(0)
        .ignore_submodules(false);
    options
}

fn hunks(diff: &Diff<'_>, index: usize) -> Result<Vec<Vec<u8>>, GitPushFailure> {
    let delta = diff.get_delta(index).ok_or(GitPushFailure::Repository)?;
    let mut hunks = Vec::new();
    if delta.old_file().mode() != delta.new_file().mode() {
        hunks.push(
            format!(
                "mode {:?} -> {:?}",
                delta.old_file().mode(),
                delta.new_file().mode()
            )
            .into_bytes(),
        );
    }
    if let Some(patch) = Patch::from_diff(diff, index).map_err(repository_failure)? {
        for index in 0..patch.num_hunks() {
            let (_, lines) = patch.hunk(index).map_err(repository_failure)?;
            let mut hunk = Vec::new();
            for line in 0..lines {
                let line = patch
                    .line_in_hunk(index, line)
                    .map_err(repository_failure)?;
                hunk.push(line.origin() as u8);
                hunk.extend_from_slice(line.content());
            }
            hunks.push(hunk);
        }
        if patch.num_hunks() > 0 || delta.old_file().id() == delta.new_file().id() {
            return Ok(hunks);
        }
    }
    // Binary and empty-file changes have no text hunks; compare object identities.
    if delta.old_file().id() != delta.new_file().id() {
        hunks.push(
            format!(
                "object {} -> {}",
                delta.old_file().id(),
                delta.new_file().id()
            )
            .into_bytes(),
        );
    }
    Ok(hunks)
}
