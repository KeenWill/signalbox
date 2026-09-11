//! Merge-forward hunk preservation before push; see tool-loop.md.

use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    time::Instant,
};

use crate::push_merge_stream::{self as streamed, Hunks};
use git2::{Delta, Diff, DiffOptions, ObjectType, Odb, Oid, Patch};
use serde::Serialize;

use crate::{
    limits::{MAX_REPOSITORY_INSPECTIONS, MAX_WORKTREE_PATH_BYTES},
    pinning::PinnedRepository,
    push_executor::{GitPushFailure, MAX_MERGE_DETAIL_BYTES},
    push_objects::ObjectSource,
};

// Use DiffFindOptions::rename_limit's documented default independently of repository config.
pub(super) const MAX_MERGE_RENAME_SOURCES: usize = 200;

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
    let mut source = ObjectSource::open(authority, Some(deadline)).map_err(repository_failure)?;
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
        trees.push((
            repository
                .find_commit(commit)
                .map_err(repository_failure)?
                .tree_id(),
            0usize,
        ));
    }
    let mut inspected_entries = 0;
    let mut path_bytes = 0;
    while let Some((oid, prefix_bytes)) = trees.pop() {
        source.capture(&database, oid).map_err(repository_failure)?;
        let tree = repository.find_tree(oid).map_err(repository_failure)?;
        if tree.len() > MAX_REPOSITORY_INSPECTIONS - inspected_entries {
            return Err(GitPushFailure::Repository);
        }
        inspected_entries += tree.len();
        // Shared tree objects still contribute entries at every path where they occur.
        for entry in &tree {
            let entry_path_bytes = prefix_bytes + entry.name_bytes().len();
            if entry_path_bytes > MAX_WORKTREE_PATH_BYTES - path_bytes {
                return Err(GitPushFailure::Repository);
            }
            path_bytes += entry_path_bytes;
            if entry.kind() == Some(ObjectType::Tree) {
                trees.push((entry.id(), entry_path_bytes + 1));
            }
        }
    }
    let tree = |oid| repository.find_commit(oid)?.tree();
    let merge_tree = tree(target).map_err(repository_failure)?;
    let base_tree = tree(base).map_err(repository_failure)?;
    let branch_tree = tree(branch).map_err(repository_failure)?;
    let ancestor_tree = tree(ancestor).map_err(repository_failure)?;
    let mut options = diff_options();
    let carried_diff = repository
        .diff_tree_to_tree(Some(&base_tree), Some(&merge_tree), Some(&mut options))
        .map_err(repository_failure)?;
    let own_diff = repository
        .diff_tree_to_tree(Some(&ancestor_tree), Some(&branch_tree), Some(&mut options))
        .map_err(repository_failure)?;
    let base_diff = repository
        .diff_tree_to_tree(Some(&ancestor_tree), Some(&base_tree), Some(&mut options))
        .map_err(repository_failure)?;
    let base_changes = detect_renames(&base_diff, &mut source, &database, deadline)?;
    let carried = detect_renames(&carried_diff, &mut source, &database, deadline)?;
    let own = detect_renames(&own_diff, &mut source, &database, deadline)?;
    let base_sources: BTreeMap<_, _> = base_changes
        .deltas()
        .filter(|delta| delta.status() == Delta::Renamed)
        .map(|delta| {
            Ok((
                delta.new_file().path().ok_or(GitPushFailure::Repository)?,
                delta.old_file().path().ok_or(GitPushFailure::Repository)?,
            ))
        })
        .collect::<Result<_, GitPushFailure>>()?;
    let base_by_path: BTreeMap<_, _> = base_changes
        .deltas()
        .enumerate()
        .map(|(index, delta)| {
            Ok((
                delta
                    .old_file()
                    .path()
                    .or_else(|| delta.new_file().path())
                    .ok_or(GitPushFailure::Repository)?,
                index,
            ))
        })
        .collect::<Result<_, GitPushFailure>>()?;
    let mut own_by_path = BTreeMap::new();
    let mut own_sources = BTreeMap::new();
    for (index, delta) in own.deltas().enumerate() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .ok_or(GitPushFailure::Repository)?;
        let source_path = if delta.status() == Delta::Renamed {
            delta.old_file().path().ok_or(GitPushFailure::Repository)?
        } else {
            path
        };
        if delta.status() == Delta::Renamed {
            own_sources.insert(path, source_path);
        }
        own_by_path.entry(source_path).or_insert(index);
    }
    let mut checked_branch = HashSet::new();
    for delta in carried.deltas() {
        streamed::check(deadline)?;
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .ok_or(GitPushFailure::Repository)?;
        let source_path = if delta.status() == Delta::Renamed {
            delta.old_file().path().ok_or(GitPushFailure::Repository)?
        } else {
            path
        };
        let source_path = if delta.status() == Delta::Added {
            own_sources.get(path).copied().unwrap_or(source_path)
        } else {
            base_sources
                .get(source_path)
                .copied()
                .unwrap_or(source_path)
        };
        let own_index = own_by_path.get(source_path).copied();
        checked_branch.extend(own_index);
        let branch_delta = own_index.and_then(|index| own.get_delta(index));
        let base_delta = base_by_path
            .get(source_path)
            .and_then(|index| base_changes.get_delta(*index));
        // An identical branch transition with an untouched base needs no blob reads.
        if base_delta.is_none() && branch_delta == Some(delta) {
            continue;
        }
        for change in std::iter::once(delta).chain(branch_delta).chain(base_delta) {
            capture_change(change, &mut source, &database)?;
        }
        let mut contents = Contents::new(&source, deadline);
        let mut branch_hunks = branch_delta
            .map(|delta| change_hunks(delta, None, &mut contents, deadline))
            .transpose()?
            .map_or_else(Hunks::new, Ok)?;
        let mut base_hunks = base_delta
            .map(|delta| change_hunks(delta, None, &mut contents, deadline))
            .transpose()?
            .map_or_else(Hunks::new, Ok)?;
        let mut carried_hunks = change_hunks(delta, Some(source_path), &mut contents, deadline)?;
        let ancestor = base_delta
            .or(branch_delta)
            .map_or(delta.old, |delta| delta.old);
        let result = Change {
            status: Delta::Modified,
            old: ancestor,
            new: delta.new,
        };
        let mut result_hunks = change_hunks(result, None, &mut contents, deadline)?;
        let mut retained = result_hunks.permitted_filtered(Some(true), deadline)?;
        let mut permitted = branch_hunks.permitted_filtered(Some(false), deadline)?;
        let mut missing = base_hunks.first_dropped_filtered(
            &mut retained,
            preview_bytes,
            Some(true),
            deadline,
        )?;
        if missing.is_none() {
            missing = carried_hunks.first_dropped_filtered(
                &mut permitted,
                preview_bytes,
                Some(false),
                deadline,
            )?;
        }
        if missing.is_none()
            && !contents.preserves(
                [
                    ancestor,
                    branch_delta.map_or(ancestor, |delta| delta.new),
                    base_delta.map_or(ancestor, |delta| delta.new),
                    delta.new,
                ],
                &branch_hunks,
                &base_hunks,
            )?
        {
            missing = carried_hunks.first_dropped(
                &mut Hunks::new()?.permitted(deadline)?,
                preview_bytes,
                deadline,
            )?;
        }
        if let Some((preview, truncated)) = missing {
            preview_bytes -= preview.len();
            dropped.insert(path.to_owned(), (preview, truncated));
        }
    }
    for (index, delta) in own.deltas().enumerate() {
        if checked_branch.contains(&index) {
            continue;
        }
        let path = delta.new_file().path().ok_or(GitPushFailure::Repository)?;
        let source_path = delta.old_file().path().ok_or(GitPushFailure::Repository)?;
        let base_delta = base_by_path
            .get(source_path)
            .and_then(|index| base_changes.get_delta(*index));
        let result_path = base_delta
            .and_then(|delta| delta.new_file().path())
            .unwrap_or(path);
        let result_entry = match merge_tree.get_path(result_path) {
            Ok(entry) => Some(entry),
            Err(error) if error.code() == git2::ErrorCode::NotFound => None,
            Err(error) => return Err(repository_failure(error)),
        };
        let result = result_entry
            .as_ref()
            .filter(|entry| entry.kind() == Some(ObjectType::Blob))
            .map_or(
                Side {
                    oid: match repository.object_format() {
                        git2::ObjectFormat::Sha1 => Oid::ZERO_SHA1,
                        git2::ObjectFormat::Sha256 => Oid::ZERO_SHA256,
                    },
                    mode: git2::FileMode::Unreadable,
                    path: Some(result_path),
                },
                |entry| Side {
                    oid: entry.id(),
                    mode: git2::FileMode::Blob,
                    path: Some(result_path),
                },
            );
        for change in std::iter::once(delta)
            .chain(base_delta)
            .chain(std::iter::once(Change {
                status: Delta::Modified,
                old: delta.old,
                new: result,
            }))
        {
            capture_change(change, &mut source, &database)?;
        }
        let mut contents = Contents::new(&source, deadline);
        let mut branch_hunks = change_hunks(delta, None, &mut contents, deadline)?;
        let base_hunks = base_delta
            .map(|delta| change_hunks(delta, None, &mut contents, deadline))
            .transpose()?
            .map_or_else(Hunks::new, Ok)?;
        if !contents.preserves(
            [
                delta.old,
                delta.new,
                base_delta.map_or(delta.old, |delta| delta.new),
                result,
            ],
            &branch_hunks,
            &base_hunks,
        )? && let Some((preview, truncated)) = branch_hunks.first_dropped(
            &mut Hunks::new()?.permitted(deadline)?,
            preview_bytes,
            deadline,
        )? {
            preview_bytes -= preview.len();
            dropped.insert(path.to_owned(), (preview, truncated));
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

fn diff_options() -> DiffOptions {
    let mut options = DiffOptions::new();
    options
        .context_lines(0)
        .interhunk_lines(0)
        .ignore_submodules(false);
    options
}

fn capture_change(
    delta: Change<'_>,
    source: &mut ObjectSource,
    database: &Odb<'_>,
) -> Result<(), GitPushFailure> {
    for file in [delta.old, delta.new] {
        if !file.id().is_zero() && file.mode() != git2::FileMode::Commit {
            source
                .capture(database, file.id())
                .map_err(repository_failure)?;
        }
    }
    Ok(())
}

// Retain decoded versions only for one path comparison; subsequent diffs clone
// descriptors, never blob bytes. No cached files accumulate across paths.
struct Contents<'a> {
    source: &'a ObjectSource,
    files: BTreeMap<Oid, crate::streamed_object::ObjectContent>,
    indexes: BTreeMap<Oid, Option<streamed::Lines>>,
    deadline: Instant,
}
impl<'a> Contents<'a> {
    fn new(source: &'a ObjectSource, deadline: Instant) -> Self {
        Self {
            source,
            files: BTreeMap::new(),
            indexes: BTreeMap::new(),
            deadline,
        }
    }
    fn lines(&mut self, side: Side<'_>) -> Result<Option<streamed::Lines>, GitPushFailure> {
        if !self.indexes.contains_key(&side.id()) {
            let content = self.get(side)?;
            self.indexes
                .insert(side.id(), streamed::Lines::new(content, self.deadline)?);
        }
        self.indexes
            .get(&side.id())
            .and_then(Option::as_ref)
            .map(streamed::Lines::duplicate)
            .transpose()
    }
    fn preserves(
        &mut self,
        versions: [Side<'_>; 4],
        branch: &Hunks,
        base: &Hunks,
    ) -> Result<bool, GitPushFailure> {
        let [ancestor, branch_side, base_side, result] = versions;
        let result = self.get(result)?;
        if branch.edits.is_empty() && base.edits.is_empty() {
            return streamed::preserves_text(
                self.get(ancestor)?,
                self.get(branch_side)?,
                self.get(base_side)?,
                result,
                branch,
                base,
                self.deadline,
            );
        }
        let Some(ancestor) = self.lines(ancestor)? else {
            return Ok(false);
        };
        let Some(branch_lines) = self.lines(branch_side)? else {
            return Ok(false);
        };
        let Some(base_lines) = self.lines(base_side)? else {
            return Ok(false);
        };
        streamed::preserves_indexed_text(
            &ancestor,
            &branch_lines,
            &base_lines,
            &result,
            branch,
            base,
            self.deadline,
        )
    }
    fn get(
        &mut self,
        side: Side<'_>,
    ) -> Result<crate::streamed_object::ObjectContent, GitPushFailure> {
        let content = match self.files.entry(side.id()) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let content = if side.id().is_zero() || side.mode() == git2::FileMode::Commit {
                    crate::streamed_object::ObjectContent::decode(
                        &mut std::io::empty(),
                        0,
                        ObjectType::Blob,
                        Some(self.deadline),
                    )
                    .map_err(repository_failure)?
                } else {
                    self.source
                        .content(side.id())
                        .map_err(repository_failure)?
                        .ok_or(GitPushFailure::Repository)?
                };
                entry.insert(content)
            }
        };
        Ok(crate::streamed_object::ObjectContent {
            file: content.file.try_clone().map_err(repository_failure)?,
            size: content.size,
            kind: content.kind,
        })
    }
}

fn change_hunks(
    delta: Change<'_>,
    rename_source: Option<&Path>,
    contents: &mut Contents<'_>,
    deadline: Instant,
) -> Result<Hunks, GitPushFailure> {
    streamed::check(deadline)?;
    let mut hunks = Hunks::new()?;
    if delta.status() == Delta::Renamed {
        let quoted = |path: Option<&Path>| {
            path.map(|path| crate::diff::quoted_diff_path(b"", path))
                .ok_or(GitPushFailure::Repository)
        };
        hunks.single(
            &[
                b"rename ".as_slice(),
                &quoted(rename_source.or_else(|| delta.old_file().path()))?,
                b" -> ",
                &quoted(delta.new_file().path())?,
            ]
            .concat(),
        )?;
    }
    if delta.old_file().mode() != delta.new_file().mode() {
        hunks.single(
            format!(
                "mode {:?} -> {:?}",
                delta.old_file().mode(),
                delta.new_file().mode()
            )
            .as_bytes(),
        )?;
    }
    if delta.old_file().id() == delta.new_file().id() {
        return Ok(hunks);
    }
    if delta.old_file().mode() == git2::FileMode::Commit
        || delta.new_file().mode() == git2::FileMode::Commit
    {
        object_hunk(delta, &mut hunks)?;
        return Ok(hunks);
    }
    let mut old = contents.get(delta.old_file())?;
    let mut new = contents.get(delta.new_file())?;
    let old_prefix = old
        .prefix(crate::limits::MAX_DIFF_BYTES)
        .map_err(repository_failure)?;
    let new_prefix = new
        .prefix(crate::limits::MAX_DIFF_BYTES)
        .map_err(repository_failure)?;
    if old_prefix.contains(&0) || new_prefix.contains(&0) {
        object_hunk(delta, &mut hunks)?;
    } else if old.size <= crate::limits::MAX_DIFF_BYTES && new.size <= crate::limits::MAX_DIFF_BYTES
    {
        let mut options = diff_options();
        let patch = Patch::from_buffers(&old_prefix, None, &new_prefix, None, Some(&mut options))
            .map_err(repository_failure)?;
        for hunk in 0..patch.num_hunks() {
            streamed::check(deadline)?;
            hunks.start()?;
            let (header, lines) = patch.hunk(hunk).map_err(repository_failure)?;
            let a = header
                .old_start()
                .saturating_sub(u32::from(header.old_lines() != 0)) as u64;
            let c = header
                .new_start()
                .saturating_sub(u32::from(header.new_lines() != 0)) as u64;
            hunks.edits.push([
                a,
                a + header.old_lines() as u64,
                c,
                c + header.new_lines() as u64,
            ]);
            for line in 0..lines {
                streamed::check(deadline)?;
                let line = patch.line_in_hunk(hunk, line).map_err(repository_failure)?;
                let mut bytes = Vec::with_capacity(line.content().len() + 1);
                bytes.push(line.origin() as u8);
                bytes.extend_from_slice(line.content());
                hunks.bytes(&bytes)?;
            }
        }
        if patch.num_hunks() == 0 {
            object_hunk(delta, &mut hunks)?;
        }
    } else {
        let detailed = match (contents.lines(delta.old)?, contents.lines(delta.new)?) {
            (Some(old), Some(new)) => {
                streamed::indexed_text_hunks(&old, &new, &mut hunks, deadline)?
                    == streamed::TextDiff::Detailed
            }
            _ => false,
        };
        if !detailed {
            object_hunk(delta, &mut hunks)?;
        }
    }

    Ok(hunks)
}
fn object_hunk(delta: Change<'_>, hunks: &mut Hunks) -> Result<(), GitPushFailure> {
    hunks.opaque = true;
    hunks.single(
        format!(
            "object {} -> {}",
            delta.old_file().id(),
            delta.new_file().id()
        )
        .as_bytes(),
    )
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Side<'a> {
    oid: Oid,
    mode: git2::FileMode,
    path: Option<&'a Path>,
}
impl<'a> Side<'a> {
    fn id(self) -> Oid {
        self.oid
    }
    fn mode(self) -> git2::FileMode {
        self.mode
    }
    fn path(self) -> Option<&'a Path> {
        self.path
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
struct Change<'a> {
    status: Delta,
    old: Side<'a>,
    new: Side<'a>,
}
impl<'a> Change<'a> {
    fn status(self) -> Delta {
        self.status
    }
    fn old_file(self) -> Side<'a> {
        self.old
    }
    fn new_file(self) -> Side<'a> {
        self.new
    }
}
struct Changes<'a>(Vec<Change<'a>>);
impl<'a> Changes<'a> {
    fn deltas(&self) -> impl Iterator<Item = Change<'a>> + '_ {
        self.0.iter().copied()
    }
    fn get_delta(&self, index: usize) -> Option<Change<'a>> {
        self.0.get(index).copied()
    }
}

fn detect_renames<'a>(
    diff: &'a Diff<'a>,
    source: &mut ObjectSource,
    database: &Odb<'_>,
    deadline: Instant,
) -> Result<Changes<'a>, GitPushFailure> {
    detect_renames_with_signatures(diff, deadline, |oid| {
        rename_signature(source, database, oid, deadline)
    })
}

fn detect_renames_with_signatures<'a>(
    diff: &'a Diff<'a>,
    deadline: Instant,
    mut signature: impl FnMut(Oid) -> Result<RenameSignature, GitPushFailure>,
) -> Result<Changes<'a>, GitPushFailure> {
    let side = |file: git2::DiffFile<'a>| Side {
        oid: file.id(),
        mode: file.mode(),
        path: file.path(),
    };
    let mut changes: Vec<_> = diff
        .deltas()
        .map(|delta| Change {
            status: delta.status(),
            old: side(delta.old_file()),
            new: side(delta.new_file()),
        })
        .collect();
    let mut paired = HashSet::new();
    let mut signatures = BTreeMap::new();
    for target in 0..changes.len() {
        streamed::check(deadline)?;
        if changes[target].status != Delta::Added {
            continue;
        }
        let new = changes[target].new;
        let mut best = None;
        let mut inspected = 0;
        for (candidate, change) in changes.iter().enumerate() {
            streamed::check(deadline)?;
            let old = change.old;
            if change.status != Delta::Deleted
                || paired.contains(&candidate)
                || (u32::from(old.mode) & 0o170000) != (u32::from(new.mode) & 0o170000)
            {
                continue;
            }
            let similarity = if old.oid == new.oid {
                100
            } else {
                if inspected >= MAX_MERGE_RENAME_SOURCES || old.mode == git2::FileMode::Commit {
                    continue;
                }
                inspected += 1;
                for oid in [new.oid, old.oid] {
                    if let std::collections::btree_map::Entry::Vacant(entry) = signatures.entry(oid)
                    {
                        entry.insert(signature(oid)?);
                    }
                }
                signatures
                    .get(&old.oid)
                    .ok_or(GitPushFailure::Repository)?
                    .similarity(signatures.get(&new.oid).ok_or(GitPushFailure::Repository)?)
            };
            // Git's default rename similarity threshold; repository config does not select it.
            if similarity >= 50 && best.is_none_or(|(_, score)| similarity > score) {
                best = Some((candidate, similarity));
            }
            if similarity == 100 {
                break;
            }
        }
        if let Some((candidate, _)) = best {
            changes[target].old = changes[candidate].old;
            changes[target].status = Delta::Renamed;
            paired.insert(candidate);
        }
    }
    Ok(Changes(
        changes
            .into_iter()
            .enumerate()
            .filter_map(|(index, change)| (!paired.contains(&index)).then_some(change))
            .collect(),
    ))
}

struct RenameSignature {
    hashes: Vec<u32>,
}
impl RenameSignature {
    fn similarity(&self, other: &Self) -> usize {
        let (mut a, mut b, mut common) = (0, 0, 0);
        while a < self.hashes.len() && b < other.hashes.len() {
            match self.hashes[a].cmp(&other.hashes[b]) {
                std::cmp::Ordering::Less => a += 1,
                std::cmp::Ordering::Greater => b += 1,
                std::cmp::Ordering::Equal => {
                    common += 1;
                    a += 1;
                    b += 1;
                }
            }
        }
        let total = self.hashes.len() + other.hashes.len();
        (common * 200usize).checked_div(total).unwrap_or(0)
    }
}
fn rename_signature(
    source: &mut ObjectSource,
    database: &Odb<'_>,
    oid: Oid,
    deadline: Instant,
) -> Result<RenameSignature, GitPushFailure> {
    use std::{
        cmp::Reverse,
        collections::BinaryHeap,
        io::{Read, Seek},
    };
    source.capture(database, oid).map_err(repository_failure)?;
    let mut content = source
        .content(oid)
        .map_err(repository_failure)?
        .ok_or(GitPushFailure::Repository)?;
    content.file.rewind().map_err(repository_failure)?;
    // Fixed-size extrema of line/run hashes retain similarity without retaining file data.
    // The run length and signature width follow libgit2's public hashsig implementation.
    const SIGNATURE_HASHES: usize = 127;
    const RUN_BYTES: usize = 80;
    const HASH_START: u64 = 0x0123_4567_8abc_def0;
    let mut low = BinaryHeap::new();
    let mut high = BinaryHeap::new();
    let mut insert = |hash: u32| {
        if low.len() < SIGNATURE_HASHES {
            low.push(hash);
        } else if low.peek().is_some_and(|largest| hash < *largest) {
            low.pop();
            low.push(hash);
        }
        if high.len() < SIGNATURE_HASHES {
            high.push(Reverse(hash));
        } else if high
            .peek()
            .is_some_and(|Reverse(smallest)| hash > *smallest)
        {
            high.pop();
            high.push(Reverse(hash));
        }
    };
    let (mut state, mut length, mut leading) = (HASH_START, 0, true);
    let mut buffer = [0; crate::streamed_object::IO_BYTES];
    loop {
        streamed::check(deadline)?;
        let count = content.file.read(&mut buffer).map_err(repository_failure)?;
        if count == 0 {
            break;
        }
        for &byte in &buffer[..count] {
            if byte == b'\r' || (leading && byte.is_ascii_whitespace() && byte != b'\n') {
                continue;
            }
            if byte == b'\n' || byte == 0 {
                if length != 0 {
                    insert(state as u32);
                }
                state = HASH_START;
                length = 0;
                leading = true;
            } else {
                leading = false;
                state = state.wrapping_mul(31).wrapping_add(u64::from(byte));
                length += 1;
                if length == RUN_BYTES {
                    insert(state as u32);
                    state = HASH_START;
                    length = 0;
                }
            }
        }
    }
    if length != 0 {
        insert(state as u32);
    }
    let mut hashes = low.into_vec();
    hashes.extend(high.into_iter().map(|Reverse(hash)| hash));
    hashes.sort_unstable();
    Ok(RenameSignature { hashes })
}

#[cfg(test)]
mod signature_tests {
    use super::*;
    use std::io::Write;

    fn compare_generated_candidates(blob_bytes: usize) {
        let root = tempfile::tempdir().expect("rename repository");
        let repository = git2::Repository::init(root.path()).expect("repository");
        let database = repository.odb().expect("object database");
        let mut before = repository.treebuilder(None).expect("before tree");
        let mut after = repository.treebuilder(None).expect("after tree");
        // Every addition compares with every deleted candidate: none shares a line signature.
        for index in 0..2 * MAX_MERGE_RENAME_SOURCES {
            let line = format!("{index:016x}\n");
            let page: Vec<_> = line.bytes().cycle().take(64 * 1024).collect();
            let mut writer = database
                .writer(blob_bytes, ObjectType::Blob)
                .expect("blob writer");
            let mut remaining = blob_bytes;
            while remaining != 0 {
                let count = remaining.min(page.len());
                writer.write_all(&page[..count]).expect("blob page");
                remaining -= count;
            }
            let oid = writer.finalize().expect("blob");
            let tree = if index < MAX_MERGE_RENAME_SOURCES {
                &mut before
            } else {
                &mut after
            };
            tree.insert(format!("candidate-{index:03}"), oid, 0o100644)
                .expect("tree entry");
        }
        let before = repository
            .find_tree(before.write().expect("before tree writes"))
            .expect("before tree");
        let after = repository
            .find_tree(after.write().expect("after tree writes"))
            .expect("after tree");
        let diff = repository
            .diff_tree_to_tree(Some(&before), Some(&after), None)
            .expect("candidate diff");
        let (_, executor) = crate::LocalGitTools::try_new(
            signalbox_tools_workspace::LocalWorkspaceFileSystem,
            root.path(),
            crate::GitIdentity::try_new("Rename fixture", "rename@example.test").expect("identity"),
        )
        .expect("local tools")
        .into_parts();
        let deadline = Instant::now() + crate::push_executor::PUSH_PREPARATION_TIMEOUT;
        let mut source = ObjectSource::open(&executor.repository_authority, Some(deadline))
            .expect("pinned objects");
        let database = Odb::new_ext(executor.repository_authority.object_format)
            .expect("private object database");
        let mut scans = BTreeMap::new();
        let changes = detect_renames_with_signatures(&diff, deadline, |oid| {
            *scans.entry(oid).or_insert(0usize) += 1;
            rename_signature(&mut source, &database, oid, deadline)
        })
        .expect("rename comparison");
        assert_eq!(changes.deltas().count(), 2 * MAX_MERGE_RENAME_SOURCES);
        assert!(
            changes
                .deltas()
                .all(|change| matches!(change.status, Delta::Added | Delta::Deleted))
        );
        assert_eq!(scans.len(), 2 * MAX_MERGE_RENAME_SOURCES);
        assert!(
            scans.values().all(|count| *count == 1),
            "each candidate blob streams once"
        );
    }

    #[test]
    fn rename_candidates_are_hashed_once_across_two_hundred_additions() {
        compare_generated_candidates(4096);
    }

    #[test]
    #[ignore = "generates and streams one gigabyte across 400 rename candidates"]
    fn rename_comparison_streams_a_generated_gigabyte_once() {
        compare_generated_candidates(2_500_000);
    }
}
