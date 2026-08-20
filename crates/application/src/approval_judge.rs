//! Authorization boundary for one dedicated approval-judge provider call.

use signalbox_domain::{
    BranchName, CommitSha, ContextFrontierId, DirectModelSelection, ModelCallId, PullRequestNumber,
    RepoWatchDispatchId, RepositorySlug, ResolvedProviderTarget, SemanticTranscriptEntryId,
    ToolRequest, TurnAttemptId,
};

/// Immutable repository-watch authority carried into a dispatched judge call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalJudgeDispatchAuthority {
    /// Exact pull-request fence the dispatch was commissioned with.
    PullRequest(ApprovalJudgePullRequestAuthority),
    /// Exact branch fence the dispatch was commissioned with.
    Branch(ApprovalJudgeBranchAuthority),
}

impl ApprovalJudgeDispatchAuthority {
    /// Returns the append-only dispatch identity that supplied this authority.
    #[must_use]
    pub const fn dispatch(&self) -> RepoWatchDispatchId {
        match self {
            Self::PullRequest(authority) => authority.dispatch(),
            Self::Branch(authority) => authority.dispatch(),
        }
    }
}

/// Labeled inputs for one immutable pull-request dispatch fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgePullRequestAuthorityInput {
    /// Append-only dispatch that commissioned the session.
    pub dispatch: RepoWatchDispatchId,
    /// Repository whose pull request commissioned the session.
    pub repository: RepositorySlug,
    /// Pull-request number within the repository.
    pub pull_request: PullRequestNumber,
    /// Exact head commit authorized at dispatch time.
    pub head_sha: CommitSha,
    /// Repository containing the authorized head branch.
    pub head_repository: RepositorySlug,
    /// Authorized head branch.
    pub head_branch: BranchName,
    /// Authorized base branch.
    pub base_branch: BranchName,
}

/// Exact pull-request authority recorded before its session became visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgePullRequestAuthority {
    input: ApprovalJudgePullRequestAuthorityInput,
}

impl ApprovalJudgePullRequestAuthority {
    /// Wraps one already-validated immutable pull-request fence.
    #[must_use]
    pub const fn new(input: ApprovalJudgePullRequestAuthorityInput) -> Self {
        Self { input }
    }

    /// Returns the append-only dispatch identity.
    #[must_use]
    pub const fn dispatch(&self) -> RepoWatchDispatchId {
        self.input.dispatch
    }

    /// Borrows the repository whose pull request was dispatched.
    #[must_use]
    pub const fn repository(&self) -> &RepositorySlug {
        &self.input.repository
    }

    /// Returns the pull-request number.
    #[must_use]
    pub const fn pull_request(&self) -> PullRequestNumber {
        self.input.pull_request
    }

    /// Borrows the exact authorized head commit.
    #[must_use]
    pub const fn head_sha(&self) -> &CommitSha {
        &self.input.head_sha
    }

    /// Borrows the repository containing the head branch.
    #[must_use]
    pub const fn head_repository(&self) -> &RepositorySlug {
        &self.input.head_repository
    }

    /// Borrows the authorized head branch.
    #[must_use]
    pub const fn head_branch(&self) -> &BranchName {
        &self.input.head_branch
    }

    /// Borrows the authorized base branch.
    #[must_use]
    pub const fn base_branch(&self) -> &BranchName {
        &self.input.base_branch
    }
}

/// Labeled inputs for one immutable branch dispatch fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeBranchAuthorityInput {
    /// Append-only dispatch that commissioned the session.
    pub dispatch: RepoWatchDispatchId,
    /// Repository whose branch commissioned the session.
    pub repository: RepositorySlug,
    /// Authorized branch.
    pub branch: BranchName,
}

/// Exact branch authority recorded before its session became visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeBranchAuthority {
    input: ApprovalJudgeBranchAuthorityInput,
}

impl ApprovalJudgeBranchAuthority {
    /// Wraps one already-validated immutable branch fence.
    #[must_use]
    pub const fn new(input: ApprovalJudgeBranchAuthorityInput) -> Self {
        Self { input }
    }

    /// Returns the append-only dispatch identity.
    #[must_use]
    pub const fn dispatch(&self) -> RepoWatchDispatchId {
        self.input.dispatch
    }

    /// Borrows the repository whose branch was dispatched.
    #[must_use]
    pub const fn repository(&self) -> &RepositorySlug {
        &self.input.repository
    }

    /// Borrows the authorized branch.
    #[must_use]
    pub const fn branch(&self) -> &BranchName {
        &self.input.branch
    }
}

/// Fresh identities for normal continuation or headless-escalation closeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApprovalJudgeCompletionIdentities {
    continuation_attempt: TurnAttemptId,
    failure_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
}

impl ApprovalJudgeCompletionIdentities {
    /// Groups identities whose uniqueness guards one completion transaction.
    #[must_use]
    pub const fn new(
        continuation_attempt: TurnAttemptId,
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            continuation_attempt,
            failure_entry,
            terminal_frontier,
        }
    }

    /// Returns the continuation attempt identity.
    #[must_use]
    pub const fn continuation_attempt(self) -> TurnAttemptId {
        self.continuation_attempt
    }

    /// Returns the terminal failure transcript-entry identity.
    #[must_use]
    pub const fn failure_entry(self) -> SemanticTranscriptEntryId {
        self.failure_entry
    }

    /// Returns the terminal context-frontier identity.
    #[must_use]
    pub const fn terminal_frontier(self) -> ContextFrontierId {
        self.terminal_frontier
    }
}

/// Exact durable binding authorized to enter an approval-judge provider.
pub trait ApprovalJudgeAuthorization {
    /// Borrows the exact parked request being judged.
    fn request(&self) -> &ToolRequest;

    /// Returns the dedicated model-call identity.
    fn call(&self) -> ModelCallId;

    /// Returns the direct selection frozen for the judge call.
    fn selection(&self) -> DirectModelSelection;

    /// Returns the exact resolved provider target.
    fn target(&self) -> ResolvedProviderTarget;

    /// Borrows the pinned non-secret credential reference.
    fn credential_reference(&self) -> &str;
}
