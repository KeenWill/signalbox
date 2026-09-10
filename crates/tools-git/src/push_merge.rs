//! Merge-forward hunk preservation before push; see tool-loop.md.

use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
    path::Path,
    time::Instant,
};

use bstr::ByteSlice;
use git2::{Delta, Diff, DiffFindOptions, DiffOptions, ObjectType, Odb, Oid, Patch};
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
        let source_path = if delta.status() == Delta::Added {
            // A branch rename may appear as add/delete against a heavily edited base.
            own_sources.get(path).copied().unwrap_or(source_path)
        } else {
            base_sources
                .get(source_path)
                .copied()
                .unwrap_or(source_path)
        };
        let own_index = own_by_path.get(source_path).copied();
        let base_index = base_by_path.get(source_path).copied();
        checked_branch.extend(own_index);
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
        let delta = carried.get_delta(index).ok_or(GitPushFailure::Repository)?;
        let branch_hunks = own_index
            .map(|index| hunks(&own, index, None))
            .transpose()?
            .unwrap_or_default();
        let base_hunks = base_index
            .map(|index| hunks(&base_changes, index, None))
            .transpose()?
            .unwrap_or_default();
        let carried_hunks = hunks(&carried, index, Some(source_path))?;
        let ancestor_delta = base_index
            .and_then(|index| base_changes.get_delta(index))
            .or_else(|| own_index.and_then(|index| own.get_delta(index)));
        let blob = |file: git2::DiffFile<'_>| {
            if file.id().is_zero() || file.mode() == git2::FileMode::Commit {
                Ok(None)
            } else {
                repository
                    .find_blob(file.id())
                    .map(Some)
                    .map_err(repository_failure)
            }
        };
        let old = ancestor_delta
            .map(|delta| blob(delta.old_file()))
            .transpose()?
            .flatten();
        let new = blob(delta.new_file())?;
        let base_blob = base_index
            .and_then(|index| base_changes.get_delta(index))
            .map(|delta| blob(delta.new_file()))
            .transpose()?
            .flatten();
        let base_path = base_index
            .and_then(|index| base_changes.get_delta(index))
            .and_then(|delta| delta.new_file().path())
            .unwrap_or(source_path);
        let generated = base_path
            .extension()
            .is_some_and(|extension| extension == "md")
            && base_blob.as_ref().is_some_and(|blob| {
                let first = blob
                    .content()
                    .split(|byte| *byte == b'\n')
                    .next()
                    .unwrap_or_default();
                first.starts_with(b"<!-- generated by ") && first.ends_with(b", do not edit -->")
            });
        let old_bytes: &[u8] = old.as_ref().map_or(&[], git2::Blob::content);
        let new_bytes: &[u8] = new.as_ref().map_or(&[], git2::Blob::content);
        let patch =
            Patch::from_buffers(old_bytes, None, new_bytes, None, Some(&mut diff_options()))
                .map_err(repository_failure)?;
        let result_hunks = text_hunks(&patch)?;
        let preserves_branch = preserves_nonconflicting_text(
            old_bytes,
            new_bytes,
            &branch_hunks,
            &base_hunks,
            generated,
        );
        let mut retained = BTreeMap::new();
        for effect in result_hunks
            .iter()
            .flat_map(Hunk::effects)
            .filter(|effect| is_text_change(effect))
        {
            *retained.entry(effect).or_insert(0usize) += 1;
        }
        let mut permitted = BTreeMap::new();
        for effect in branch_hunks
            .iter()
            .flat_map(Hunk::effects)
            .filter(|effect| !is_text_change(effect))
        {
            *permitted.entry(effect).or_insert(0usize) += 1;
        }
        let missing_base = base_hunks.iter().find(|hunk| {
            if permits_regenerated_hunk(source_path, generated, hunk, &branch_hunks, &result_hunks)
            {
                return false;
            }
            !hunk
                .effects()
                .filter(|effect| is_text_change(effect))
                .all(|effect| consume_effect(&mut retained, effect))
        });
        let invalid_metadata = || {
            carried_hunks.iter().find(|hunk| {
                !hunk
                    .effects()
                    .filter(|effect| !is_text_change(effect))
                    .all(|effect| consume_effect(&mut permitted, effect))
            })
        };
        if let Some(hunk) = missing_base
            .or_else(invalid_metadata)
            .or_else(|| (!preserves_branch).then(|| carried_hunks.first()).flatten())
        {
            record_dropped_hunk(&mut dropped, &mut preview_bytes, path, hunk);
        }
    }
    for (index, delta) in own.deltas().enumerate() {
        if checked_branch.contains(&index) {
            continue;
        }
        let path = delta.new_file().path().ok_or(GitPushFailure::Repository)?;
        let source_path = delta.old_file().path().ok_or(GitPushFailure::Repository)?;
        let base_index = base_by_path.get(source_path).copied();
        let base_delta = base_index.and_then(|index| base_changes.get_delta(index));
        let result_path = base_delta
            .as_ref()
            .and_then(|delta| delta.new_file().path())
            .unwrap_or(path);
        let result_entry = match merge_tree.get_path(result_path) {
            Ok(entry) => Some(entry),
            Err(error) if error.code() == git2::ErrorCode::NotFound => None,
            Err(error) => return Err(repository_failure(error)),
        };
        let mut old = None;
        if !delta.old_file().id().is_zero() && delta.old_file().mode() != git2::FileMode::Commit {
            source
                .capture(&database, delta.old_file().id())
                .map_err(repository_failure)?;
            old = Some(
                repository
                    .find_blob(delta.old_file().id())
                    .map_err(repository_failure)?,
            );
        }
        let mut new = None;
        if let Some(entry) = result_entry.filter(|entry| entry.kind() == Some(ObjectType::Blob)) {
            source
                .capture(&database, entry.id())
                .map_err(repository_failure)?;
            new = Some(
                repository
                    .find_blob(entry.id())
                    .map_err(repository_failure)?,
            );
        }
        for delta in std::iter::once(delta).chain(base_delta) {
            for file in [delta.old_file(), delta.new_file()] {
                if !file.id().is_zero() && file.mode() != git2::FileMode::Commit {
                    source
                        .capture(&database, file.id())
                        .map_err(repository_failure)?;
                }
            }
        }
        let branch_hunks = hunks(&own, index, None)?;
        let base_hunks = base_index
            .map(|index| hunks(&base_changes, index, None))
            .transpose()?
            .unwrap_or_default();
        if !preserves_nonconflicting_text(
            old.as_ref().map_or(&[], git2::Blob::content),
            new.as_ref().map_or(&[], git2::Blob::content),
            &branch_hunks,
            &base_hunks,
            false,
        ) && let Some(hunk) = branch_hunks.first()
        {
            record_dropped_hunk(&mut dropped, &mut preview_bytes, path, hunk);
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

fn record_dropped_hunk(
    dropped: &mut BTreeMap<std::path::PathBuf, (String, bool)>,
    preview_bytes: &mut usize,
    path: &Path,
    hunk: &Hunk,
) {
    let prefix = hunk.bytes.len().min(*preview_bytes);
    let (preview, shortened) = crate::bounded::bounded_bytes(&hunk.bytes[..prefix], *preview_bytes);
    let truncated = shortened || prefix < hunk.bytes.len();
    *preview_bytes -= preview.len();
    dropped.insert(path.to_owned(), (preview, truncated));
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
    diff.find_similar(Some(
        DiffFindOptions::new()
            .renames(true)
            .rename_limit(MAX_MERGE_RENAME_SOURCES),
    ))
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
    old_lines: Option<Range<usize>>,
}

impl Hunk {
    fn single(bytes: Vec<u8>) -> Self {
        let end = bytes.len();
        Self {
            bytes,
            changes: std::iter::once(0..end).collect(),
            old_lines: None,
        }
    }

    fn effects(&self) -> impl Iterator<Item = &[u8]> {
        self.changes.iter().map(|range| &self.bytes[range.clone()])
    }
}

fn permits_regenerated_hunk(
    path: &Path,
    generated: bool,
    base: &Hunk,
    branch: &[Hunk],
    result: &[Hunk],
) -> bool {
    let Some(base_range) = &base.old_lines else {
        return false;
    };
    let test_source = path.extension().is_some_and(|extension| extension == "rs")
        && (path
            .components()
            .any(|component| component.as_os_str() == "tests")
            || path.file_stem().is_some_and(|stem| {
                stem == "tests" || stem.to_str().is_some_and(|stem| stem.ends_with("_tests"))
            }));
    branch.iter().any(|own| {
        own.old_lines
            .as_ref()
            .is_some_and(|range| overlapping_edits(range, base_range))
            && (generated || !own.effects().eq(base.effects()))
            && (generated
                || (test_source
                    && base.effects().all(is_json_fixture_literal)
                    && own.effects().all(is_json_fixture_literal)
                    && result.iter().any(|hunk| {
                        hunk.old_lines
                            .as_ref()
                            .is_some_and(|range| overlapping_edits(range, base_range))
                            && hunk.effects().all(is_json_fixture_literal)
                            && hunk.effects().any(|effect| effect.starts_with(b"+"))
                    })))
    })
}

fn overlapping_edits(left: &Range<usize>, right: &Range<usize>) -> bool {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => left.start == right.start,
        (true, false) => right.start < left.start && left.start < right.end,
        (false, true) => left.start < right.start && right.start < left.end,
        (false, false) => left.start < right.end && right.start < left.end,
    }
}

fn is_json_fixture_literal(effect: &[u8]) -> bool {
    let Some(line) = effect
        .strip_prefix(b"+")
        .or_else(|| effect.strip_prefix(b"-"))
    else {
        return false;
    };
    let line = line.trim_ascii();
    let line = line.strip_suffix(b",").unwrap_or(line);
    let Some(raw) = line.strip_prefix(b"r") else {
        return false;
    };
    let hashes = raw.iter().take_while(|byte| **byte == b'#').count();
    let Some(quoted) = raw.get(hashes..).and_then(|raw| raw.strip_prefix(b"\"")) else {
        return false;
    };
    let Some(end) = quoted.len().checked_sub(hashes + 1) else {
        return false;
    };
    let (literal, closing) = quoted.split_at(end);
    if closing.first() != Some(&b'"')
        || !closing[1..].iter().all(|byte| *byte == b'#')
        || literal
            .windows(closing.len())
            .any(|window| window == closing)
    {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(literal).is_ok()
}

// Non-conflicting spans are literal; only overlapping parent edits admit new text.
fn preserves_nonconflicting_text(
    ancestor: &[u8],
    result: &[u8],
    branch: &[Hunk],
    base: &[Hunk],
    generated: bool,
) -> bool {
    let lines: Vec<_> = ancestor.split_inclusive(|byte| *byte == b'\n').collect();
    let mut edits: Vec<_> = branch
        .iter()
        .map(|hunk| (false, hunk))
        .chain(base.iter().map(|hunk| (true, hunk)))
        .filter_map(|(side, hunk)| hunk.old_lines.as_ref().map(|range| (range, side, hunk)))
        .collect();
    if edits.is_empty() {
        return ancestor == result
            || branch
                .iter()
                .chain(base)
                .flat_map(Hunk::effects)
                .any(|effect| effect.starts_with(b"object "));
    }
    edits.sort_by_key(|(range, _, _)| (range.start, range.end));
    let mut fixed = Vec::new();
    let mut spans = Vec::new();
    let mut cursor = 0;
    let mut index = 0;
    while index < edits.len() {
        let start = edits[index].0.start;
        let mut end = edits[index].0.end;
        let first = index;
        index += 1;
        while index < edits.len() && overlapping_edits(&(start..end), edits[index].0) {
            end = end.max(edits[index].0.end);
            index += 1;
        }
        for line in &lines[cursor..start] {
            fixed.extend_from_slice(line);
        }
        let region = &edits[first..index];
        let replacement = |side| {
            let mut bytes = Vec::new();
            let mut cursor = start;
            for (range, _, hunk) in region.iter().filter(|(_, source, _)| *source == side) {
                for line in &lines[cursor..range.start] {
                    bytes.extend_from_slice(line);
                }
                for effect in hunk
                    .effects()
                    .filter(|effect| effect.first() == Some(&b'+'))
                {
                    bytes.extend_from_slice(&effect[1..]);
                }
                cursor = range.end;
            }
            for line in &lines[cursor..end] {
                bytes.extend_from_slice(line);
            }
            bytes
        };
        let branch_changed = region.iter().any(|(_, side, _)| !side);
        let base_changed = region.iter().any(|(_, side, _)| *side);
        let branch_text = replacement(false);
        let base_text = replacement(true);
        if branch_changed && base_changed && (generated || branch_text != base_text) {
            spans.push(std::mem::take(&mut fixed));
        } else {
            fixed.extend_from_slice(if branch_changed {
                &branch_text
            } else {
                &base_text
            });
        }
        cursor = end;
    }
    for line in &lines[cursor..] {
        fixed.extend_from_slice(line);
    }
    if spans.is_empty() {
        return result == fixed;
    }
    let Some(mut remainder) = result.strip_prefix(spans[0].as_slice()) else {
        return false;
    };
    for span in &spans[1..] {
        if span.is_empty() {
            continue;
        }
        let Some(offset) = remainder.find(span) else {
            return false;
        };
        remainder = &remainder[offset + span.len()..];
    }
    remainder.ends_with(&fixed)
}

fn is_text_change(effect: &[u8]) -> bool {
    matches!(effect.first(), Some(b'+' | b'-'))
}

fn consume_effect(permitted: &mut BTreeMap<&[u8], usize>, effect: &[u8]) -> bool {
    let Some(remaining) = permitted.get_mut(effect) else {
        return false;
    };
    if *remaining == 0 {
        return false;
    }
    *remaining -= 1;
    true
}

fn text_hunks(patch: &Patch<'_>) -> Result<Vec<Hunk>, GitPushFailure> {
    let mut hunks = Vec::new();
    for index in 0..patch.num_hunks() {
        let (header, lines) = patch.hunk(index).map_err(repository_failure)?;
        let start = header
            .old_start()
            .saturating_sub(u32::from(header.old_lines() != 0)) as usize;
        let mut hunk = Hunk {
            old_lines: Some(start..start + header.old_lines() as usize),
            ..Hunk::default()
        };
        for line in 0..lines {
            let line = patch
                .line_in_hunk(index, line)
                .map_err(repository_failure)?;
            let start = hunk.bytes.len();
            hunk.bytes.push(line.origin() as u8);
            hunk.bytes.extend_from_slice(line.content());
            if matches!(line.origin(), '+' | '-') {
                hunk.changes.push(start..hunk.bytes.len());
            }
        }
        hunks.push(hunk);
    }
    Ok(hunks)
}

fn hunks(
    diff: &Diff<'_>,
    index: usize,
    rename_source: Option<&Path>,
) -> Result<Vec<Hunk>, GitPushFailure> {
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
                &quoted(rename_source.or_else(|| delta.old_file().path()))?,
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
        hunks.extend(text_hunks(&patch)?);
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
