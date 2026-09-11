//! Repository watch event for `docs/spec/repo-watch.md`.

use super::dispatch::RepoWatchDispatchContextShape;
use super::values::{
    BranchName, CheckRunName, CommitSha, GitHubObjectId, LabelName, PullRequestBody,
    PullRequestNumber, PullRequestTitle, ReactionContent, RepoWatchAuthorLogin, RepositorySlug,
    ReviewThreadId, WorkflowName,
};
use crate::RepoWatchEventId;
use std::{error::Error, fmt, hash::Hash};

/// Closed version-one event-kind discriminator vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RepoWatchEventKindNameV1 {
    PullRequestOpened,
    PullRequestClosed,
    PullRequestMerged,
    HeadChanged,
    MergeableStateChanged,
    ChecksCompleted,
    CheckRunCompleted,
    BranchWorkflowRunCompleted,
    ReviewSubmitted,
    ThreadOpened,
    ThreadResolved,
    Labeled,
    Unlabeled,
    BaseAdvanced,
    ReactionChanged,
}

/// Aggregate check-suite result used by the closed version-one vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChecksOutcome {
    Success,
    Failure,
}

/// Closed provider conclusion vocabulary admitted by version one.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Stale,
    StartupFailure,
}

impl From<ChecksOutcome> for CheckConclusion {
    fn from(value: ChecksOutcome) -> Self {
        match value {
            ChecksOutcome::Success => Self::Success,
            ChecksOutcome::Failure => Self::Failure,
        }
    }
}

/// GitHub's mergeability classification retained by the differ.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

/// Review state relevant to repository-watch rules and dispatch context.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
}

/// Whether a configured reviewer's reaction was added or removed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReactionChange {
    Added,
    Removed,
}

/// The object on which a configured reviewer's reaction changed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReactionSubject {
    PullRequestBody,
    IssueComment { id: GitHubObjectId },
    ReviewComment { id: GitHubObjectId },
}

/// Closed version-one repository-watch fact payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchEventKindV1 {
    PullRequestOpened,
    PullRequestClosed,
    PullRequestMerged,
    HeadChanged {
        previous: CommitSha,
        current: CommitSha,
    },
    MergeableStateChanged {
        current: MergeableState,
    },
    ChecksCompleted {
        outcome: ChecksOutcome,
    },
    CheckRunCompleted {
        name: CheckRunName,
        conclusion: CheckConclusion,
    },
    BranchWorkflowRunCompleted {
        branch: BranchName,
        workflow: WorkflowName,
        conclusion: CheckConclusion,
    },
    ReviewSubmitted {
        reviewer: RepoWatchAuthorLogin,
        state: ReviewState,
        commit: CommitSha,
    },
    ThreadOpened {
        thread: ReviewThreadId,
        author: Option<RepoWatchAuthorLogin>,
    },
    ThreadResolved {
        thread: ReviewThreadId,
        author: Option<RepoWatchAuthorLogin>,
    },
    Labeled {
        label: LabelName,
    },
    Unlabeled {
        label: LabelName,
    },
    BaseAdvanced {
        branch: BranchName,
    },
    ReactionChanged {
        subject: ReactionSubject,
        reactor: RepoWatchAuthorLogin,
        content: ReactionContent,
        change: ReactionChange,
    },
}

impl RepoWatchEventKindV1 {
    pub const fn name(&self) -> RepoWatchEventKindNameV1 {
        match self {
            Self::PullRequestOpened => RepoWatchEventKindNameV1::PullRequestOpened,
            Self::PullRequestClosed => RepoWatchEventKindNameV1::PullRequestClosed,
            Self::PullRequestMerged => RepoWatchEventKindNameV1::PullRequestMerged,
            Self::HeadChanged { .. } => RepoWatchEventKindNameV1::HeadChanged,
            Self::MergeableStateChanged { .. } => RepoWatchEventKindNameV1::MergeableStateChanged,
            Self::ChecksCompleted { .. } => RepoWatchEventKindNameV1::ChecksCompleted,
            Self::CheckRunCompleted { .. } => RepoWatchEventKindNameV1::CheckRunCompleted,
            Self::BranchWorkflowRunCompleted { .. } => {
                RepoWatchEventKindNameV1::BranchWorkflowRunCompleted
            }
            Self::ReviewSubmitted { .. } => RepoWatchEventKindNameV1::ReviewSubmitted,
            Self::ThreadOpened { .. } => RepoWatchEventKindNameV1::ThreadOpened,
            Self::ThreadResolved { .. } => RepoWatchEventKindNameV1::ThreadResolved,
            Self::Labeled { .. } => RepoWatchEventKindNameV1::Labeled,
            Self::Unlabeled { .. } => RepoWatchEventKindNameV1::Unlabeled,
            Self::BaseAdvanced { .. } => RepoWatchEventKindNameV1::BaseAdvanced,
            Self::ReactionChanged { .. } => RepoWatchEventKindNameV1::ReactionChanged,
        }
    }
}

/// Complete normalized pull-request facts available to version-one matchers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestEventContext {
    number: PullRequestNumber,
    head_sha: CommitSha,
    head_repository: RepositorySlug,
    base_branch: BranchName,
    head_branch: BranchName,
    title: PullRequestTitle,
    body: PullRequestBody,
    labels: Box<[LabelName]>,
    draft: bool,
    author: Option<RepoWatchAuthorLogin>,
}

/// Field-labeled construction input for normalized pull-request event context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestEventContextInput {
    pub number: PullRequestNumber,
    pub head_sha: CommitSha,
    pub head_repository: RepositorySlug,
    pub base_branch: BranchName,
    pub head_branch: BranchName,
    pub title: PullRequestTitle,
    pub body: PullRequestBody,
    pub labels: Vec<LabelName>,
    pub draft: bool,
    pub author: Option<RepoWatchAuthorLogin>,
}

impl PullRequestEventContext {
    pub fn new(input: PullRequestEventContextInput) -> Self {
        let mut labels = input.labels;
        labels.sort();
        labels.dedup();
        Self {
            number: input.number,
            head_sha: input.head_sha,
            head_repository: input.head_repository,
            base_branch: input.base_branch,
            head_branch: input.head_branch,
            title: input.title,
            body: input.body,
            labels: labels.into_boxed_slice(),
            draft: input.draft,
            author: input.author,
        }
    }

    pub const fn number(&self) -> PullRequestNumber {
        self.number
    }
    pub const fn head_sha(&self) -> &CommitSha {
        &self.head_sha
    }
    pub const fn head_repository(&self) -> &RepositorySlug {
        &self.head_repository
    }
    pub const fn base_branch(&self) -> &BranchName {
        &self.base_branch
    }
    pub const fn head_branch(&self) -> &BranchName {
        &self.head_branch
    }
    pub const fn title(&self) -> &PullRequestTitle {
        &self.title
    }
    pub const fn body(&self) -> &PullRequestBody {
        &self.body
    }
    pub fn labels(&self) -> &[LabelName] {
        &self.labels
    }
    pub const fn draft(&self) -> bool {
        self.draft
    }
    pub const fn author(&self) -> Option<&RepoWatchAuthorLogin> {
        self.author.as_ref()
    }
}

/// The subject shape carried by one version-one event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchEventTarget {
    PullRequest(PullRequestEventContext),
    Branch,
}

/// One version-one durable repository-watch fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchEvent {
    id: RepoWatchEventId,
    repository: RepositorySlug,
    target: RepoWatchEventTarget,
    kind: RepoWatchEventKindV1,
}

impl RepoWatchEvent {
    pub fn try_pull_request(
        id: RepoWatchEventId,
        repository: RepositorySlug,
        context: PullRequestEventContext,
        kind: RepoWatchEventKindV1,
    ) -> Result<Self, RepoWatchEventConstructionError> {
        match &kind {
            RepoWatchEventKindV1::BranchWorkflowRunCompleted { .. } => {
                return Err(RepoWatchEventConstructionError::BranchKindOnPullRequest);
            }
            RepoWatchEventKindV1::HeadChanged { previous, current } if previous == current => {
                return Err(RepoWatchEventConstructionError::HeadChangedWithoutChange);
            }
            RepoWatchEventKindV1::HeadChanged { current, .. } if current != context.head_sha() => {
                return Err(RepoWatchEventConstructionError::HeadChangedCurrentMismatch);
            }
            RepoWatchEventKindV1::BaseAdvanced { branch } if branch != context.base_branch() => {
                return Err(RepoWatchEventConstructionError::BaseAdvancedBranchMismatch);
            }
            RepoWatchEventKindV1::Labeled { label } if !context.labels().contains(label) => {
                return Err(RepoWatchEventConstructionError::LabeledContextMissingLabel);
            }
            RepoWatchEventKindV1::Unlabeled { label } if context.labels().contains(label) => {
                return Err(RepoWatchEventConstructionError::UnlabeledContextContainsLabel);
            }
            RepoWatchEventKindV1::PullRequestOpened
            | RepoWatchEventKindV1::PullRequestClosed
            | RepoWatchEventKindV1::PullRequestMerged
            | RepoWatchEventKindV1::HeadChanged { .. }
            | RepoWatchEventKindV1::MergeableStateChanged { .. }
            | RepoWatchEventKindV1::ChecksCompleted { .. }
            | RepoWatchEventKindV1::CheckRunCompleted { .. }
            | RepoWatchEventKindV1::ReviewSubmitted { .. }
            | RepoWatchEventKindV1::ThreadOpened { .. }
            | RepoWatchEventKindV1::ThreadResolved { .. }
            | RepoWatchEventKindV1::Labeled { .. }
            | RepoWatchEventKindV1::Unlabeled { .. }
            | RepoWatchEventKindV1::BaseAdvanced { .. }
            | RepoWatchEventKindV1::ReactionChanged { .. } => {}
        }
        Ok(Self {
            id,
            repository,
            target: RepoWatchEventTarget::PullRequest(context),
            kind,
        })
    }

    pub const fn branch_workflow(
        id: RepoWatchEventId,
        repository: RepositorySlug,
        branch: BranchName,
        workflow: WorkflowName,
        conclusion: CheckConclusion,
    ) -> Self {
        Self {
            id,
            repository,
            target: RepoWatchEventTarget::Branch,
            kind: RepoWatchEventKindV1::BranchWorkflowRunCompleted {
                branch,
                workflow,
                conclusion,
            },
        }
    }

    pub const fn id(&self) -> RepoWatchEventId {
        self.id
    }
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }
    pub const fn target(&self) -> &RepoWatchEventTarget {
        &self.target
    }
    pub const fn kind(&self) -> &RepoWatchEventKindV1 {
        &self.kind
    }
}

/// Why an event target and event kind could not be combined.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchEventConstructionError {
    BranchKindOnPullRequest,
    HeadChangedCurrentMismatch,
    HeadChangedWithoutChange,
    BaseAdvancedBranchMismatch,
    LabeledContextMissingLabel,
    UnlabeledContextContainsLabel,
}

impl fmt::Display for RepoWatchEventConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BranchKindOnPullRequest => {
                "branch-workflow event cannot carry a pull-request target"
            }
            Self::HeadChangedCurrentMismatch => {
                "head-change current SHA differs from pull-request context"
            }
            Self::HeadChangedWithoutChange => "head-change previous and current SHAs are identical",
            Self::BaseAdvancedBranchMismatch => {
                "base-advance branch differs from pull-request context"
            }
            Self::LabeledContextMissingLabel => {
                "labeled event label is absent from pull-request context"
            }
            Self::UnlabeledContextContainsLabel => {
                "unlabeled event label remains in pull-request context"
            }
        })
    }
}

impl Error for RepoWatchEventConstructionError {}

impl RepoWatchEventKindNameV1 {
    /// Every event-kind name, in an inventory the compiler forces to be
    /// revisited and a paired `inventory_predecessor` forces to stay linked.
    ///
    /// That pairing is test-gated, so it is named here in plain text rather
    /// than linked: a rustdoc link would resolve only under `cfg(test)` and
    /// break the documentation build.
    ///
    /// The chain below is the first guard: each arm names its successor, so
    /// adding a variant makes the `match` non-exhaustive and the crate stops
    /// compiling until the new name is slotted into it.
    ///
    /// Exhaustiveness alone constrains the *arms*, not reachability from the
    /// head: a variant added as `NewKind => None` that no arm points at
    /// compiles while never appearing in the returned list. The paired
    /// `inventory_predecessor` match is the second guard — it is exhaustive
    /// for the same reason, and `every_event_kind_is_linked_into_the_inventory`
    /// checks the two are mutual inverses, so a link written in one direction
    /// only fails rather than silently shortening the inventory.
    ///
    /// The residual limit is recorded rather than papered over: a variant
    /// orphaned in *both* directions still cannot be detected here, because
    /// safe Rust offers no way to enumerate an enum's variants without a
    /// derive, and this crate deliberately takes no `strum`/`EnumIter`
    /// dependency. Closing that last case is a dependency decision, not a
    /// code change. A hand-written `vec![..]` is what `docs/style.md` forbids
    /// and would be strictly worse: it goes stale with no compiler signal at
    /// all, whereas this cannot change shape without the author editing two
    /// exhaustive matches.
    #[must_use]
    pub fn all() -> Vec<Self> {
        let mut names = Vec::new();
        let mut next = Some(Self::PullRequestOpened);
        while let Some(current) = next {
            next = match current {
                Self::PullRequestOpened => Some(Self::PullRequestClosed),
                Self::PullRequestClosed => Some(Self::PullRequestMerged),
                Self::PullRequestMerged => Some(Self::HeadChanged),
                Self::HeadChanged => Some(Self::MergeableStateChanged),
                Self::MergeableStateChanged => Some(Self::ChecksCompleted),
                Self::ChecksCompleted => Some(Self::CheckRunCompleted),
                Self::CheckRunCompleted => Some(Self::BranchWorkflowRunCompleted),
                Self::BranchWorkflowRunCompleted => Some(Self::ReviewSubmitted),
                Self::ReviewSubmitted => Some(Self::ThreadOpened),
                Self::ThreadOpened => Some(Self::ThreadResolved),
                Self::ThreadResolved => Some(Self::Labeled),
                Self::Labeled => Some(Self::Unlabeled),
                Self::Unlabeled => Some(Self::BaseAdvanced),
                Self::BaseAdvanced => Some(Self::ReactionChanged),
                Self::ReactionChanged => None,
            };
            names.push(current);
        }
        names
    }

    /// The inventory predecessor of `self`, or `None` for the head.
    ///
    /// Paired with the successor chain in [`Self::all`] so linkage is checked
    /// rather than assumed. Exhaustive for the same reason that one is, and
    /// test-gated because checking the pairing is its only purpose — CI always
    /// compiles the tests, so a new variant still cannot skip this match.
    #[cfg(test)]
    pub(super) const fn inventory_predecessor(self) -> Option<Self> {
        match self {
            Self::PullRequestOpened => None,
            Self::PullRequestClosed => Some(Self::PullRequestOpened),
            Self::PullRequestMerged => Some(Self::PullRequestClosed),
            Self::HeadChanged => Some(Self::PullRequestMerged),
            Self::MergeableStateChanged => Some(Self::HeadChanged),
            Self::ChecksCompleted => Some(Self::MergeableStateChanged),
            Self::CheckRunCompleted => Some(Self::ChecksCompleted),
            Self::BranchWorkflowRunCompleted => Some(Self::CheckRunCompleted),
            Self::ReviewSubmitted => Some(Self::BranchWorkflowRunCompleted),
            Self::ThreadOpened => Some(Self::ReviewSubmitted),
            Self::ThreadResolved => Some(Self::ThreadOpened),
            Self::Labeled => Some(Self::ThreadResolved),
            Self::Unlabeled => Some(Self::Labeled),
            Self::BaseAdvanced => Some(Self::Unlabeled),
            Self::ReactionChanged => Some(Self::BaseAdvanced),
        }
    }

    pub(super) const fn dispatch_context_shape(self) -> RepoWatchDispatchContextShape {
        match self {
            Self::PullRequestOpened
            | Self::PullRequestClosed
            | Self::PullRequestMerged
            | Self::HeadChanged
            | Self::MergeableStateChanged
            | Self::ChecksCompleted
            | Self::CheckRunCompleted
            | Self::ReviewSubmitted
            | Self::ThreadOpened
            | Self::ThreadResolved
            | Self::Labeled
            | Self::Unlabeled
            | Self::BaseAdvanced
            | Self::ReactionChanged => RepoWatchDispatchContextShape::PullRequest,
            Self::BranchWorkflowRunCompleted => RepoWatchDispatchContextShape::Branch,
        }
    }
}
