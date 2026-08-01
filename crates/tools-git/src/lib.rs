//! Typed repository-local Git tools over an injected workspace root.
//!
//! Repository discovery and linked worktrees are deliberately unsupported. A
//! suite binds one direct main worktree whose .git directory is inside the
//! injected root. The local family has no remote operation.

mod arguments;
mod bounded;
mod branch;
mod catalog;
mod commit;
mod construction;
mod contracts;
mod decode;
mod descriptor;
mod diff;
mod executor;
mod failure;
mod identity;
mod index_lock;
mod layout;
mod limits;
mod log;
mod names;
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
pub use catalog::LocalGitTools;
pub use construction::LocalGitToolsConstructionError;
pub use executor::{LocalGitExecutor, LocalGitExecutorError};
pub use identity::{GitIdentity, InvalidGitIdentity};
pub use names::{
    GIT_BRANCH_CREATE_NAME, GIT_BRANCH_SWITCH_NAME, GIT_CREATE_COMMIT_NAME, GIT_DIFF_NAME,
    GIT_LOG_NAME, GIT_STAGE_NAME, GIT_STATUS_NAME, LOCAL_GIT_TOOL_NAMES,
};
