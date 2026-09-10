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
        own_by_path.entry(source_path).or_insert(index);
    }
    for (index, delta) in carried.deltas().enumerate() {
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
        let source_path = base_sources
            .get(source_path)
            .copied()
            .unwrap_or(source_path);
        let own_index = own_by_path.get(source_path).copied();
        let base_index = base_by_path.get(source_path).copied();
        // Capture compared paths only, in addition to the rename candidates.
        for delta in std::iter::once(delta)
            .chain(own_index.and_then(|index| own.get_delta(index)))
            .chain(base_index.and_then(|index| base_changes.get_delta(index)))
        {
            for file in [delta.old_file(), delta.new_file()] {
                if !file.id().is_zero() && file.mode() != git2::FileMode::Commit {
                    source
                        .capture(&database, file.id())
                        .map_err(repository_failure)?;
                }
            }
        }
        let mut branch_hunks = match own_index {
            Some(index) => hunks(&own, index, None, &source, deadline)?,
            None => Hunks::new()?,
        };
        let mut base_hunks = match base_index {
            Some(index) => hunks(&base_changes, index, None, &source, deadline)?,
            None => Hunks::new()?,
        };
        let mut result_hunks = match base_index {
            Some(index) => {
                let base_delta = base_changes
                    .get_delta(index)
                    .ok_or(GitPushFailure::Repository)?;
                let result = Changes(vec![Change {
                    status: Delta::Modified,
                    old: base_delta.old_file(),
                    new: delta.new_file(),
                }]);
                hunks(&result, 0, None, &source, deadline)?
            }
            None => Hunks::new()?,
        };
        let mut retained = result_hunks.permitted(streamed::EffectsToMatch::Text, deadline)?;
        let mut permitted = branch_hunks.permitted(streamed::EffectsToMatch::Metadata, deadline)?;
        let missing_base = base_hunks.first_dropped(
            &mut retained,
            streamed::EffectsToMatch::Text,
            preview_bytes,
            deadline,
        )?;
        let invalid = match missing_base {
            Some(detail) => Some(detail),
            None => hunks(&carried, index, Some(source_path), &source, deadline)?.first_dropped(
                &mut permitted,
                streamed::EffectsToMatch::Metadata,
                preview_bytes,
                deadline,
            )?,
        };
        if let Some((preview, truncated)) = invalid {
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

fn hunks(
    diff: &Changes<'_>,
    index: usize,
    rename_source: Option<&Path>,
    source: &ObjectSource,
    deadline: Instant,
) -> Result<Hunks, GitPushFailure> {
    streamed::check(deadline)?;
    let delta = diff.get_delta(index).ok_or(GitPushFailure::Repository)?;
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
    let content =
        |file: Side<'_>| -> Result<crate::streamed_object::ObjectContent, GitPushFailure> {
            if file.id().is_zero() {
                crate::streamed_object::ObjectContent::decode(
                    &mut std::io::empty(),
                    0,
                    ObjectType::Blob,
                    Some(deadline),
                )
                .map_err(repository_failure)
            } else {
                source
                    .content(file.id())
                    .map_err(repository_failure)?
                    .ok_or(GitPushFailure::Repository)
            }
        };
    let mut old = content(delta.old_file())?;
    let mut new = content(delta.new_file())?;
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
            let (_, lines) = patch.hunk(hunk).map_err(repository_failure)?;
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
        streamed::text_hunks(old, new, &mut hunks, deadline)?;
    }
    Ok(hunks)
}
fn object_hunk(delta: Change<'_>, hunks: &mut Hunks) -> Result<(), GitPushFailure> {
    hunks.single(
        format!(
            "object {} -> {}",
            delta.old_file().id(),
            delta.new_file().id()
        )
        .as_bytes(),
    )
}

#[derive(Clone, Copy)]
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
#[derive(Clone, Copy)]
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
