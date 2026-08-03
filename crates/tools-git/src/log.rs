use std::collections::HashSet;

use git2::Repository;

use crate::arguments::GitLogArguments;
use crate::bounded::{
    bounded_bytes, find_bounded_commit, resolve_bounded_commit, validate_object_header,
};
use crate::failure::LocalGitFailure;
use crate::layout::validate_live_shallow;
use crate::limits::{MAX_LOG_IDENTITY_BYTES, MAX_LOG_MESSAGE_BYTES, MAX_WORKTREE_INSPECTIONS};
use crate::pinning::PinnedRepository;
use crate::result::{LogEntry, LogResult};

pub(super) fn log(
    repository: &Repository,
    authority: &PinnedRepository,
    arguments: GitLogArguments,
) -> Result<LogResult, LocalGitFailure> {
    let (start, revision_snapshot) =
        resolve_bounded_commit(repository, authority, &arguments.revision)?;
    let start = start.id();
    validate_live_shallow(&authority.git_directory, authority.object_format)?;
    let shallow = HashSet::new();
    let (ordered, truncated) =
        bounded_topological_page(repository, start, arguments.max_entries, &shallow)?;
    let mut commits = Vec::new();
    for oid in ordered {
        let commit = find_bounded_commit(repository, oid)?;
        let author = commit.author();
        let (author_name, author_name_truncated) =
            bounded_bytes(author.name_bytes(), MAX_LOG_IDENTITY_BYTES);
        let (author_email, author_email_truncated) =
            bounded_bytes(author.email_bytes(), MAX_LOG_IDENTITY_BYTES);
        let (message, message_truncated) =
            bounded_bytes(commit.message_raw_bytes(), MAX_LOG_MESSAGE_BYTES);
        commits.push(LogEntry {
            commit: oid.to_string(),
            author_name,
            author_name_truncated,
            author_email,
            author_email_truncated,
            message,
            message_truncated,
        });
    }
    let result = LogResult { commits, truncated };
    revision_snapshot.validate(authority)?;
    validate_live_shallow(&authority.git_directory, authority.object_format)?;
    Ok(result)
}

pub(super) fn bounded_topological_page(
    repository: &Repository,
    start: git2::Oid,
    limit: usize,
    shallow: &HashSet<git2::Oid>,
) -> Result<(Vec<git2::Oid>, bool), LocalGitFailure> {
    let mut frontier = vec![start];
    let mut queued = HashSet::from([start]);
    let mut emitted = HashSet::new();
    let mut ordered = Vec::with_capacity(limit);
    let mut topology_inspections = 0_usize;
    while !frontier.is_empty() && ordered.len() < limit {
        let selected = select_topological_candidate(
            repository,
            &frontier,
            shallow,
            &mut topology_inspections,
        )?;
        let oid = frontier.remove(selected);
        queued.remove(&oid);
        if !emitted.insert(oid) {
            continue;
        }
        let commit = find_bounded_commit(repository, oid)?;
        let parents = if shallow.contains(&oid) {
            Vec::new()
        } else {
            commit.parent_ids().collect::<Vec<_>>()
        };
        ordered.push(oid);
        for parent in parents {
            if validate_object_header(repository, parent)? != git2::ObjectType::Commit {
                return Err(LocalGitFailure::Operation);
            }
            if !emitted.contains(&parent) && queued.insert(parent) {
                frontier.push(parent);
            }
        }
    }
    let truncated = !frontier.is_empty();
    Ok((ordered, truncated))
}

pub(super) fn select_topological_candidate(
    repository: &Repository,
    frontier: &[git2::Oid],
    shallow: &HashSet<git2::Oid>,
    inspections: &mut usize,
) -> Result<usize, LocalGitFailure> {
    for candidate_index in 0..frontier.len() {
        let candidate = frontier[candidate_index];
        let mut is_ancestor = false;
        for (other_index, other) in frontier.iter().copied().enumerate() {
            if candidate_index != other_index
                && bounded_commit_reaches(repository, other, candidate, shallow, inspections)?
            {
                is_ancestor = true;
                break;
            }
        }
        if !is_ancestor {
            return Ok(candidate_index);
        }
    }
    Err(LocalGitFailure::Operation)
}

pub(super) fn bounded_commit_reaches(
    repository: &Repository,
    descendant: git2::Oid,
    ancestor: git2::Oid,
    shallow: &HashSet<git2::Oid>,
    inspections: &mut usize,
) -> Result<bool, LocalGitFailure> {
    let mut pending = vec![descendant];
    let mut visited = HashSet::new();
    while let Some(oid) = pending.pop() {
        if oid == ancestor {
            return Ok(true);
        }
        if !visited.insert(oid) {
            continue;
        }
        *inspections = inspections
            .checked_add(1)
            .filter(|count| *count <= MAX_WORKTREE_INSPECTIONS)
            .ok_or(LocalGitFailure::Operation)?;
        let commit = find_bounded_commit(repository, oid)?;
        if !shallow.contains(&oid) {
            pending.extend(commit.parent_ids());
        }
    }
    Ok(false)
}
