//! Merge parent ancestry verification before push; see tool-loop.md.

use std::{collections::BTreeSet, time::Instant};

use git2::Oid;

use crate::{
    limits::MAX_REPOSITORY_INSPECTIONS, pinning::PinnedRepository, push_executor::GitPushFailure,
    push_objects::ObjectSource,
};

fn repository_failure<T>(_: T) -> GitPushFailure {
    GitPushFailure::Repository
}

pub(super) fn verify_merge(
    authority: &PinnedRepository,
    target: Oid,
    fence: Option<Oid>,
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
    if merge.parent_count() > 2 {
        return Err(GitPushFailure::UnsupportedMergeShape {
            parents: merge.parent_count(),
        });
    }
    if merge.parent_count() < 2 {
        return Ok(());
    }
    let fence = fence.ok_or(GitPushFailure::UnprovenMergeParents)?;
    let mut commits: Vec<_> = merge.parent_ids().collect();
    let mut visited = BTreeSet::new();
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
    if !visited.contains(&fence) {
        return Err(GitPushFailure::UnprovenMergeParents);
    }
    let first = merge.parent_id(0).map_err(repository_failure)?;
    let second = merge.parent_id(1).map_err(repository_failure)?;
    let contains_fence = |parent| {
        if parent == fence {
            Ok(true)
        } else {
            repository
                .graph_descendant_of(parent, fence)
                .map_err(repository_failure)
        }
    };
    if contains_fence(first)? == contains_fence(second)? {
        return Err(GitPushFailure::UnprovenMergeParents);
    }
    let ancestors = repository
        .merge_bases(first, second)
        .map_err(repository_failure)?;
    if ancestors.len() > 1 {
        let mut bases = ancestors.to_vec();
        bases.sort_unstable();
        return Err(GitPushFailure::AmbiguousMergeBases { bases });
    }
    if ancestors.is_empty() {
        return Err(GitPushFailure::Repository);
    }
    source.validate(authority).map_err(repository_failure)
}
