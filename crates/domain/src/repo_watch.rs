//! Repository-watch events, matchers, and dispatch action values.
//!
//! The normative cross-component contract is `docs/spec/repo-watch.md`.

mod action;
mod dispatch;
mod event;
mod matcher;
mod rule;
mod values;

#[cfg(test)]
mod tests;

pub use action::RepoWatchRuleActionV1;
pub use dispatch::{RepoWatchDispatchContextShape, RepoWatchSingletonScope};
pub use event::{
    CheckConclusion, ChecksOutcome, MergeableState, PullRequestEventContext,
    PullRequestEventContextInput, ReactionChange, ReactionSubject, RepoWatchEvent,
    RepoWatchEventConstructionError, RepoWatchEventKindNameV1, RepoWatchEventKindV1,
    RepoWatchEventTarget, ReviewState,
};
pub use matcher::{
    RepoWatchLabelMatcher, RepoWatchLabelMatcherInput, RepoWatchMatcherV1, RepoWatchMatcherV1Input,
};
pub use rule::{
    RepoWatchRule, RepoWatchRuleContentDigest, RepoWatchRuleIdentityField,
    RepoWatchRuleIdentityFieldDigest, RepoWatchRuleValidationError,
};
pub use values::{
    BranchName, CheckRunName, CommitSha, GitHubObjectId, LabelName, PullRequestBody,
    PullRequestNumber, PullRequestTitle, ReactionContent, RepoWatchAuthorLogin, RepoWatchPattern,
    RepoWatchRuleId, RepoWatchRuleVersion, RepoWatchTextError, RepoWatchWorkflowRunAttempt,
    RepositorySlug, ReviewThreadId, WorkflowName,
};
