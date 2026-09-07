//! Repository watch dispatch for `docs/spec/repo-watch.md`.

use std::{fmt, hash::Hash};

/// Durable singleton key selected independently by each rule.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum RepoWatchSingletonScope {
    #[default]
    PullRequest,
    Stack,
    Rule,
    Repository,
}

impl RepoWatchSingletonScope {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::PullRequest => "pull_request",
            Self::Stack => "stack",
            Self::Rule => "rule",
            Self::Repository => "repo",
        }
    }
}

/// Context shape a session template explicitly accepts.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RepoWatchDispatchContextShape {
    PullRequest,
    Branch,
}

impl fmt::Display for RepoWatchDispatchContextShape {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PullRequest => "pull-request",
            Self::Branch => "branch",
        })
    }
}
