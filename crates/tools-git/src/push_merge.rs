//! Merge-forward hunk preservation before push; see tool-loop.md.

use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
    path::Path,
    time::Instant,
};

use git2::{Delta, Diff, DiffFindOptions, DiffOptions, ObjectType, Odb, Oid, Patch};
use serde::Serialize;

use crate::{
    limits::MAX_REPOSITORY_INSPECTIONS,
    pinning::PinnedRepository,
    push_executor::{GitPushFailure, MAX_MERGE_DETAIL_BYTES},
    push_objects::ObjectSource,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct DroppedBaseChanges {
    pub(super) file: String,
    pub(super) first_dropped_hunk: String,
    pub(super) truncated: bool,
}

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
    let (branch, base) = match (contains_fence(first)?, contains_fence(second)?) {
        (true, false) => (first, second),
        (false, true) => (second, first),
        _ => return Err(GitPushFailure::UnprovenMergeParents),
    };
    let ancestors = repository
        .merge_bases(branch, base)
        .map_err(repository_failure)?;
    if ancestors.len() > 1 {
        let mut bases = ancestors.to_vec();
        bases.sort_unstable();
        return Err(GitPushFailure::AmbiguousMergeBases { bases });
    }
    let ancestor = *ancestors.first().ok_or(GitPushFailure::Repository)?;
    let mut dropped = BTreeMap::new();
    let mut preview_bytes = MAX_MERGE_DETAIL_BYTES;
    let mut trees = Vec::new();
    for commit in [target, branch, base, ancestor] {
        trees.push(
            repository
                .find_commit(commit)
                .map_err(repository_failure)?
                .tree_id(),
        );
    }
    let mut inspected_entries = 0;
    while let Some(oid) = trees.pop() {
        source.capture(&database, oid).map_err(repository_failure)?;
        let tree = repository.find_tree(oid).map_err(repository_failure)?;
        if tree.len() > MAX_REPOSITORY_INSPECTIONS - inspected_entries {
            return Err(GitPushFailure::Repository);
        }
        inspected_entries += tree.len();
        // Shared tree objects still contribute entries at every path where they occur.
        for entry in &tree {
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
    let mut carried = repository
        .diff_tree_to_tree(Some(&base_tree), Some(&merge_tree), Some(&mut options))
        .map_err(repository_failure)?;
    let mut own = repository
        .diff_tree_to_tree(Some(&ancestor_tree), Some(&branch_tree), Some(&mut options))
        .map_err(repository_failure)?;
    let mut base_changes = repository
        .diff_tree_to_tree(Some(&ancestor_tree), Some(&base_tree), Some(&mut options))
        .map_err(repository_failure)?;
    detect_renames(&mut base_changes, &mut source, &database)?;
    detect_renames(&mut carried, &mut source, &database)?;
    detect_renames(&mut own, &mut source, &database)?;
    let base_renames: BTreeMap<_, _> = base_changes
        .deltas()
        .filter(|delta| delta.status() == Delta::Renamed)
        .map(|delta| {
            Ok((
                delta.old_file().path().ok_or(GitPushFailure::Repository)?,
                delta.new_file().path().ok_or(GitPushFailure::Repository)?,
            ))
        })
        .collect::<Result<_, GitPushFailure>>()?;
    let mut own_by_path = BTreeMap::new();
    for (index, delta) in own.deltas().enumerate() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .ok_or(GitPushFailure::Repository)?;
        let path = base_renames.get(path).copied().unwrap_or(path);
        own_by_path.entry(path).or_insert(index);
    }
    for (index, delta) in carried.deltas().enumerate() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .ok_or(GitPushFailure::Repository)?;
        let own_index = own_by_path.get(path).copied();
        // Capture compared paths only, in addition to the rename candidates.
        for delta in std::iter::once(delta).chain(own_index.and_then(|index| own.get_delta(index)))
        {
            for file in [delta.old_file(), delta.new_file()] {
                if !file.id().is_zero() && file.mode() != git2::FileMode::Commit {
                    source
                        .capture(&database, file.id())
                        .map_err(repository_failure)?;
                }
            }
        }
        let branch_hunks = own_index
            .map(|index| hunks(&own, index))
            .transpose()?
            .unwrap_or_default();
        let mut permitted = BTreeMap::new();
        for effect in branch_hunks.iter().flat_map(Hunk::effects) {
            *permitted.entry(effect).or_insert(0usize) += 1;
        }
        for hunk in hunks(&carried, index)? {
            let carried = hunk.effects().all(|effect| {
                let Some(remaining) = permitted.get_mut(effect) else {
                    return false;
                };
                if *remaining == 0 {
                    return false;
                }
                *remaining -= 1;
                true
            });
            if !carried {
                let prefix = hunk.bytes.len().min(preview_bytes);
                let (preview, shortened) =
                    crate::bounded::bounded_bytes(&hunk.bytes[..prefix], preview_bytes);
                let truncated = shortened || prefix < hunk.bytes.len();
                preview_bytes -= preview.len();
                dropped.insert(path.to_owned(), (preview, truncated));
                break;
            }
        }
    }
    source.validate(authority).map_err(repository_failure)?;
    if dropped.is_empty() {
        Ok(())
    } else {
        Err(GitPushFailure::MergeDroppedBaseChanges(
            dropped
                .into_iter()
                .map(|(path, (first_dropped_hunk, truncated))| {
                    Ok(DroppedBaseChanges {
                        file: String::from_utf8(crate::diff::quoted_diff_path(b"", &path))
                            .map_err(repository_failure)?,
                        first_dropped_hunk,
                        truncated,
                    })
                })
                .collect::<Result<_, GitPushFailure>>()?,
        ))
    }
}

fn detect_renames(
    diff: &mut Diff<'_>,
    source: &mut ObjectSource,
    database: &Odb<'_>,
) -> Result<(), GitPushFailure> {
    for delta in diff
        .deltas()
        .filter(|delta| matches!(delta.status(), Delta::Added | Delta::Deleted))
    {
        for file in [delta.old_file(), delta.new_file()] {
            if !file.id().is_zero() && file.mode() != git2::FileMode::Commit {
                source
                    .capture(database, file.id())
                    .map_err(repository_failure)?;
            }
        }
    }
    diff.find_similar(Some(DiffFindOptions::new().renames(true)))
        .map_err(repository_failure)
}

fn diff_options() -> DiffOptions {
    let mut options = DiffOptions::new();
    options
        .context_lines(0)
        .interhunk_lines(0)
        .ignore_submodules(false);
    options
}

#[derive(Default)]
struct Hunk {
    bytes: Vec<u8>,
    changes: Vec<Range<usize>>,
}

impl Hunk {
    fn single(bytes: Vec<u8>) -> Self {
        let end = bytes.len();
        Self {
            bytes,
            changes: std::iter::once(0..end).collect(),
        }
    }

    fn effects(&self) -> impl Iterator<Item = &[u8]> {
        self.changes.iter().map(|range| &self.bytes[range.clone()])
    }
}

fn hunks(diff: &Diff<'_>, index: usize) -> Result<Vec<Hunk>, GitPushFailure> {
    let delta = diff.get_delta(index).ok_or(GitPushFailure::Repository)?;
    let mut hunks = Vec::new();
    if delta.status() == Delta::Renamed {
        let quoted = |path: Option<&Path>| {
            path.map(|path| crate::diff::quoted_diff_path(b"", path))
                .ok_or(GitPushFailure::Repository)
        };
        hunks.push(Hunk::single(
            [
                b"rename ".as_slice(),
                &quoted(delta.old_file().path())?,
                b" -> ",
                &quoted(delta.new_file().path())?,
            ]
            .concat(),
        ));
    }
    if delta.old_file().mode() != delta.new_file().mode() {
        hunks.push(Hunk::single(
            format!(
                "mode {:?} -> {:?}",
                delta.old_file().mode(),
                delta.new_file().mode()
            )
            .into_bytes(),
        ));
    }
    if let Some(patch) = Patch::from_diff(diff, index).map_err(repository_failure)? {
        for index in 0..patch.num_hunks() {
            let (_, lines) = patch.hunk(index).map_err(repository_failure)?;
            let mut hunk = Hunk::default();
            for line in 0..lines {
                let line = patch
                    .line_in_hunk(index, line)
                    .map_err(repository_failure)?;
                let start = hunk.bytes.len();
                hunk.bytes.push(line.origin() as u8);
                hunk.bytes.extend_from_slice(line.content());
                hunk.changes.push(start..hunk.bytes.len());
            }
            hunks.push(hunk);
        }
        if patch.num_hunks() > 0 || delta.old_file().id() == delta.new_file().id() {
            return Ok(hunks);
        }
    }
    // Binary and empty-file changes have no text hunks; compare object identities.
    if delta.old_file().id() != delta.new_file().id() {
        hunks.push(Hunk::single(
            format!(
                "object {} -> {}",
                delta.old_file().id(),
                delta.new_file().id()
            )
            .into_bytes(),
        ));
    }
    Ok(hunks)
}
