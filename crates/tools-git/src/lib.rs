//! Typed repository-local Git tools over an injected workspace root.
//!
//! Repository discovery and linked worktrees are deliberately unsupported. A
//! suite binds one direct main worktree whose .git directory is inside the
//! injected root. The local family has no remote operation.
#![allow(dead_code)]

mod arguments;
mod bounded;
mod branch;
mod commit;
mod construction;
mod descriptor;
mod diff;
mod executor;
mod failure;
mod identity;
mod index_lock;
mod layout;
mod limits;
mod log;
mod objects;
mod pack_install;
mod packed_reference;
mod pinning;
mod reference_lock;
mod reference_read;
mod reflog;
mod result;
mod rollback;
mod status;
mod status_reference;
#[cfg(test)]
mod tests;

pub use arguments::{
    GitBranchCreateArguments, GitBranchSwitchArguments, GitCommitArguments, GitDiffArguments,
    GitLogArguments, GitStageArguments, GitStatusArguments, InvalidGitArguments,
};
pub use construction::LocalGitToolsConstructionError;
pub use executor::{LocalGitExecutor, LocalGitExecutorError};
pub use identity::{GitIdentity, InvalidGitIdentity};
