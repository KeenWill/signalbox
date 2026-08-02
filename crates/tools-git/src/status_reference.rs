use crate::bounded::bounded_bytes;
use crate::failure::LocalGitFailure;
use crate::limits::MAX_BRANCH_BYTES;
use crate::pinning::PinnedRepository;
use crate::reference_read::resolve_pinned_reference_chain_from;
#[cfg(test)]
use git2::ErrorCode;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StatusHeadSnapshot {
    chain: Vec<String>,
    pub(super) branch: Option<String>,
    pub(super) branch_truncated: bool,
    pub(super) target: Option<git2::Oid>,
}

impl StatusHeadSnapshot {
    pub(super) fn capture(authority: &PinnedRepository) -> Result<Self, LocalGitFailure> {
        let (chain, target) = resolve_pinned_reference_chain_from(authority, "HEAD", None)
            .map_err(|_| LocalGitFailure::Operation)?;
        let branch = chain
            .get(1)
            .and_then(|name| name.strip_prefix("refs/heads/"));
        let (branch, branch_truncated) = match branch {
            Some(branch) => {
                let (branch, truncated) = bounded_bytes(branch.as_bytes(), MAX_BRANCH_BYTES);
                (Some(branch), truncated)
            }
            None => (None, false),
        };
        Ok(Self {
            chain,
            branch,
            branch_truncated,
            target,
        })
    }

    pub(super) fn validate(&self, authority: &PinnedRepository) -> Result<(), LocalGitFailure> {
        if Self::capture(authority)? == *self {
            Ok(())
        } else {
            Err(LocalGitFailure::Operation)
        }
    }
}

pub(super) fn status_head(
    authority: &PinnedRepository,
) -> Result<(Option<String>, bool, Option<git2::Oid>), LocalGitFailure> {
    let snapshot = StatusHeadSnapshot::capture(authority)?;
    Ok((snapshot.branch, snapshot.branch_truncated, snapshot.target))
}

#[cfg(test)]
pub(super) fn status_head_from_reference(
    head: &git2::Reference<'_>,
) -> Result<(Option<String>, bool, Option<git2::Oid>), LocalGitFailure> {
    let branch = head
        .symbolic_target_bytes()
        .and_then(|target| target.strip_prefix(b"refs/heads/"));
    let (branch, branch_truncated) = match branch {
        Some(branch) => {
            let (branch, truncated) = bounded_bytes(branch, MAX_BRANCH_BYTES);
            (Some(branch), truncated)
        }
        None => (None, false),
    };
    let target = match head.target() {
        Some(target) => Some(target),
        None => match head.resolve() {
            Ok(resolved) => Some(resolved.target().ok_or(LocalGitFailure::Operation)?),
            Err(error) if error.code() == ErrorCode::NotFound => None,
            Err(_) => return Err(LocalGitFailure::Operation),
        },
    };
    Ok((branch, branch_truncated, target))
}
