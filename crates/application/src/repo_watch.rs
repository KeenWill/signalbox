//! Pure repository-state comparison for the repository-watch event boundary.

use std::{error::Error, fmt};

use signalbox_domain::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, GitHubObjectId,
    MergeableState, PullRequestEventContext, PullRequestNumber, ReactionChange, ReactionContent,
    ReactionSubject, RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventConstructionError,
    RepoWatchEventId, RepoWatchEventKindV1, RepositorySlug, ReviewState, ReviewThreadId,
    WorkflowName,
};

/// Supplies identities in the exact order in which the differ emits facts.
pub trait RepoWatchEventIdGenerator {
    fn next_event_id(&mut self) -> RepoWatchEventId;
}

/// Production repository-watch event identity source.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7RepoWatchEventIdGenerator;

impl RepoWatchEventIdGenerator for UuidV7RepoWatchEventIdGenerator {
    fn next_event_id(&mut self) -> RepoWatchEventId {
        RepoWatchEventId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Provider lifecycle projection for one known pull request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchPullRequestLifecycle {
    Open,
    Closed,
    Merged,
}

/// One completed check-suite identity and its aggregate outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchCheckSuiteObservation {
    id: GitHubObjectId,
    outcome: ChecksOutcome,
}

impl RepoWatchCheckSuiteObservation {
    pub const fn new(id: GitHubObjectId, outcome: ChecksOutcome) -> Self {
        Self { id, outcome }
    }

    pub const fn id(&self) -> GitHubObjectId {
        self.id
    }

    pub const fn outcome(&self) -> ChecksOutcome {
        self.outcome
    }
}

/// One completed check-run identity and its rule-visible result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchCheckRunObservation {
    id: GitHubObjectId,
    name: CheckRunName,
    conclusion: CheckConclusion,
}

impl RepoWatchCheckRunObservation {
    pub const fn new(id: GitHubObjectId, name: CheckRunName, conclusion: CheckConclusion) -> Self {
        Self {
            id,
            name,
            conclusion,
        }
    }

    pub const fn id(&self) -> GitHubObjectId {
        self.id
    }

    pub const fn name(&self) -> &CheckRunName {
        &self.name
    }

    pub const fn conclusion(&self) -> CheckConclusion {
        self.conclusion
    }
}

/// One provider review identity retained even after a later dismissal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchReviewObservation {
    id: GitHubObjectId,
    reviewer: RepoWatchAuthorLogin,
    state: Option<ReviewState>,
    commit: CommitSha,
}

impl RepoWatchReviewObservation {
    pub const fn new(
        id: GitHubObjectId,
        reviewer: RepoWatchAuthorLogin,
        state: Option<ReviewState>,
        commit: CommitSha,
    ) -> Self {
        Self {
            id,
            reviewer,
            state,
            commit,
        }
    }

    pub const fn id(&self) -> GitHubObjectId {
        self.id
    }

    pub const fn reviewer(&self) -> &RepoWatchAuthorLogin {
        &self.reviewer
    }

    pub const fn state(&self) -> Option<ReviewState> {
        self.state
    }

    pub const fn commit(&self) -> &CommitSha {
        &self.commit
    }
}

/// Current resolution state of one review thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchThreadState {
    Open,
    Resolved,
}

/// One current review-thread projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchThreadObservation {
    thread: ReviewThreadId,
    state: RepoWatchThreadState,
}

impl RepoWatchThreadObservation {
    pub const fn new(thread: ReviewThreadId, state: RepoWatchThreadState) -> Self {
        Self { thread, state }
    }

    pub const fn thread(&self) -> &ReviewThreadId {
        &self.thread
    }

    pub const fn state(&self) -> RepoWatchThreadState {
        self.state
    }
}

/// One configured reviewer's retained reaction projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchReactionObservation {
    subject: ReactionSubject,
    reactor: RepoWatchAuthorLogin,
    content: ReactionContent,
}

impl RepoWatchReactionObservation {
    pub const fn new(
        subject: ReactionSubject,
        reactor: RepoWatchAuthorLogin,
        content: ReactionContent,
    ) -> Self {
        Self {
            subject,
            reactor,
            content,
        }
    }

    pub const fn subject(&self) -> ReactionSubject {
        self.subject
    }

    pub const fn reactor(&self) -> &RepoWatchAuthorLogin {
        &self.reactor
    }

    pub const fn content(&self) -> &ReactionContent {
        &self.content
    }
}

/// Field-labeled construction input for one complete pull-request baseline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchPullRequestStateInput {
    pub context: PullRequestEventContext,
    pub lifecycle: RepoWatchPullRequestLifecycle,
    pub mergeable_state: MergeableState,
    pub completed_check_suites: Vec<RepoWatchCheckSuiteObservation>,
    pub completed_check_runs: Vec<RepoWatchCheckRunObservation>,
    pub reviews: Vec<RepoWatchReviewObservation>,
    pub threads: Vec<RepoWatchThreadObservation>,
    pub reactions: Vec<RepoWatchReactionObservation>,
}

/// Complete normalized comparison baseline for one pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchPullRequestState {
    context: PullRequestEventContext,
    lifecycle: RepoWatchPullRequestLifecycle,
    mergeable_state: MergeableState,
    completed_check_suites: Box<[RepoWatchCheckSuiteObservation]>,
    completed_check_runs: Box<[RepoWatchCheckRunObservation]>,
    reviews: Box<[RepoWatchReviewObservation]>,
    threads: Box<[RepoWatchThreadObservation]>,
    reactions: Box<[RepoWatchReactionObservation]>,
}

impl RepoWatchPullRequestState {
    pub fn try_new(
        mut input: RepoWatchPullRequestStateInput,
    ) -> Result<Self, RepoWatchRepositoryStateError> {
        input
            .completed_check_suites
            .sort_by_key(RepoWatchCheckSuiteObservation::id);
        reject_duplicate_object_ids(
            &input.completed_check_suites,
            RepoWatchCheckSuiteObservation::id,
            RepoWatchRepositoryStateError::DuplicateCheckSuite,
        )?;
        input
            .completed_check_runs
            .sort_by_key(RepoWatchCheckRunObservation::id);
        reject_duplicate_object_ids(
            &input.completed_check_runs,
            RepoWatchCheckRunObservation::id,
            RepoWatchRepositoryStateError::DuplicateCheckRun,
        )?;
        input.reviews.sort_by_key(RepoWatchReviewObservation::id);
        reject_duplicate_object_ids(
            &input.reviews,
            RepoWatchReviewObservation::id,
            RepoWatchRepositoryStateError::DuplicateReview,
        )?;
        input
            .threads
            .sort_by(|left, right| left.thread().cmp(right.thread()));
        reject_duplicate_threads(&input.threads)?;
        input.reactions.sort_by(|left, right| {
            (
                reaction_subject_sort_key(left.subject()),
                left.reactor(),
                left.content(),
            )
                .cmp(&(
                    reaction_subject_sort_key(right.subject()),
                    right.reactor(),
                    right.content(),
                ))
        });
        input.reactions.dedup();
        Ok(Self {
            context: input.context,
            lifecycle: input.lifecycle,
            mergeable_state: input.mergeable_state,
            completed_check_suites: input.completed_check_suites.into_boxed_slice(),
            completed_check_runs: input.completed_check_runs.into_boxed_slice(),
            reviews: input.reviews.into_boxed_slice(),
            threads: input.threads.into_boxed_slice(),
            reactions: input.reactions.into_boxed_slice(),
        })
    }

    pub const fn context(&self) -> &PullRequestEventContext {
        &self.context
    }

    pub const fn lifecycle(&self) -> RepoWatchPullRequestLifecycle {
        self.lifecycle
    }

    pub const fn mergeable_state(&self) -> MergeableState {
        self.mergeable_state
    }

    pub fn completed_check_suites(&self) -> &[RepoWatchCheckSuiteObservation] {
        &self.completed_check_suites
    }

    pub fn completed_check_runs(&self) -> &[RepoWatchCheckRunObservation] {
        &self.completed_check_runs
    }

    pub fn reviews(&self) -> &[RepoWatchReviewObservation] {
        &self.reviews
    }

    pub fn threads(&self) -> &[RepoWatchThreadObservation] {
        &self.threads
    }

    pub fn reactions(&self) -> &[RepoWatchReactionObservation] {
        &self.reactions
    }
}

const fn reaction_subject_sort_key(subject: ReactionSubject) -> (u8, u64) {
    match subject {
        ReactionSubject::PullRequestBody => (0, 0),
        ReactionSubject::IssueComment { id } => (1, id.get()),
        ReactionSubject::ReviewComment { id } => (2, id.get()),
    }
}

/// One latest completed branch-workflow run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchWorkflowRunObservation {
    id: GitHubObjectId,
    branch: BranchName,
    workflow: WorkflowName,
    conclusion: CheckConclusion,
}

impl RepoWatchWorkflowRunObservation {
    pub const fn new(
        id: GitHubObjectId,
        branch: BranchName,
        workflow: WorkflowName,
        conclusion: CheckConclusion,
    ) -> Self {
        Self {
            id,
            branch,
            workflow,
            conclusion,
        }
    }

    pub const fn id(&self) -> GitHubObjectId {
        self.id
    }

    pub const fn branch(&self) -> &BranchName {
        &self.branch
    }

    pub const fn workflow(&self) -> &WorkflowName {
        &self.workflow
    }

    pub const fn conclusion(&self) -> CheckConclusion {
        self.conclusion
    }
}

/// One current branch head used to derive `BaseAdvanced` facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchBranchHead {
    branch: BranchName,
    head: CommitSha,
}

impl RepoWatchBranchHead {
    pub const fn new(branch: BranchName, head: CommitSha) -> Self {
        Self { branch, head }
    }

    pub const fn branch(&self) -> &BranchName {
        &self.branch
    }

    pub const fn head(&self) -> &CommitSha {
        &self.head
    }
}

/// Field-labeled construction input for one complete normalized repository state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepoWatchRepositoryStateInput {
    pub pull_requests: Vec<RepoWatchPullRequestState>,
    pub workflow_runs: Vec<RepoWatchWorkflowRunObservation>,
    pub branch_heads: Vec<RepoWatchBranchHead>,
}

/// Complete normalized repository state retained in the durable cursor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepoWatchRepositoryState {
    pull_requests: Box<[RepoWatchPullRequestState]>,
    workflow_runs: Box<[RepoWatchWorkflowRunObservation]>,
    branch_heads: Box<[RepoWatchBranchHead]>,
}

impl RepoWatchRepositoryState {
    pub fn try_new(
        mut input: RepoWatchRepositoryStateInput,
    ) -> Result<Self, RepoWatchRepositoryStateError> {
        input
            .pull_requests
            .sort_by_key(|pull_request| pull_request.context().number());
        reject_duplicate_pull_requests(&input.pull_requests)?;
        input.workflow_runs.sort_by(|left, right| {
            (left.branch(), left.workflow()).cmp(&(right.branch(), right.workflow()))
        });
        reject_duplicate_workflows(&input.workflow_runs)?;
        input
            .branch_heads
            .sort_by(|left, right| left.branch().cmp(right.branch()));
        reject_duplicate_branch_heads(&input.branch_heads)?;
        Ok(Self {
            pull_requests: input.pull_requests.into_boxed_slice(),
            workflow_runs: input.workflow_runs.into_boxed_slice(),
            branch_heads: input.branch_heads.into_boxed_slice(),
        })
    }

    pub fn pull_requests(&self) -> &[RepoWatchPullRequestState] {
        &self.pull_requests
    }

    pub fn workflow_runs(&self) -> &[RepoWatchWorkflowRunObservation] {
        &self.workflow_runs
    }

    pub fn branch_heads(&self) -> &[RepoWatchBranchHead] {
        &self.branch_heads
    }
}

/// One accepted comparison baseline and its exact reaction-filter provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchObservation {
    signal_reviewers: Box<[RepoWatchAuthorLogin]>,
    state: RepoWatchRepositoryState,
}

impl RepoWatchObservation {
    pub fn new(
        mut signal_reviewers: Vec<RepoWatchAuthorLogin>,
        state: RepoWatchRepositoryState,
    ) -> Self {
        signal_reviewers.sort();
        signal_reviewers.dedup();
        Self {
            signal_reviewers: signal_reviewers.into_boxed_slice(),
            state,
        }
    }

    pub fn signal_reviewers(&self) -> &[RepoWatchAuthorLogin] {
        &self.signal_reviewers
    }

    pub const fn state(&self) -> &RepoWatchRepositoryState {
        &self.state
    }
}

/// Why normalized repository state could not be made canonical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchRepositoryStateError {
    DuplicatePullRequest(PullRequestNumber),
    DuplicateCheckSuite(GitHubObjectId),
    DuplicateCheckRun(GitHubObjectId),
    DuplicateReview(GitHubObjectId),
    DuplicateThread(ReviewThreadId),
    DuplicateWorkflow {
        branch: BranchName,
        workflow: WorkflowName,
    },
    DuplicateBranchHead(BranchName),
}

impl fmt::Display for RepoWatchRepositoryStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePullRequest(number) => {
                write!(formatter, "duplicate pull request {}", number.get())
            }
            Self::DuplicateCheckSuite(id) => {
                write!(formatter, "duplicate check suite {}", id.get())
            }
            Self::DuplicateCheckRun(id) => write!(formatter, "duplicate check run {}", id.get()),
            Self::DuplicateReview(id) => write!(formatter, "duplicate review {}", id.get()),
            Self::DuplicateThread(thread) => {
                write!(formatter, "duplicate review thread {}", thread.as_str())
            }
            Self::DuplicateWorkflow { branch, workflow } => write!(
                formatter,
                "duplicate branch workflow {}/{}",
                branch.as_str(),
                workflow.as_str()
            ),
            Self::DuplicateBranchHead(branch) => {
                write!(formatter, "duplicate branch head {}", branch.as_str())
            }
        }
    }
}

impl Error for RepoWatchRepositoryStateError {}

/// Internal coherence failure while deriving a domain event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchDifferError(RepoWatchEventConstructionError);

impl fmt::Display for RepoWatchDifferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "repository-watch differ produced an invalid event: {}",
            self.0
        )
    }
}

impl Error for RepoWatchDifferError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// Compares consecutive accepted normalized observations into closed domain facts.
pub fn derive_repo_watch_events(
    repository: &RepositorySlug,
    previous: Option<&RepoWatchObservation>,
    current: &RepoWatchObservation,
    ids: &mut impl RepoWatchEventIdGenerator,
) -> Result<Vec<RepoWatchEvent>, RepoWatchDifferError> {
    let mut events = Vec::new();
    let reaction_filter_unchanged =
        previous.is_none_or(|prior| prior.signal_reviewers() == current.signal_reviewers());
    for current_pull_request in current.state().pull_requests() {
        let previous_pull_request = previous.and_then(|prior| {
            find_pull_request(
                prior.state().pull_requests(),
                current_pull_request.context().number(),
            )
        });
        derive_pull_request_events(
            repository,
            previous_pull_request,
            current_pull_request,
            RepositoryComparison {
                previous: previous.map(RepoWatchObservation::state),
                current: current.state(),
                reaction_filter_unchanged,
            },
            ids,
            &mut events,
        )?;
    }
    if let Some(previous) = previous {
        derive_workflow_events(
            repository,
            previous.state(),
            current.state(),
            ids,
            &mut events,
        );
    }
    Ok(events)
}

#[derive(Clone, Copy)]
struct RepositoryComparison<'a> {
    previous: Option<&'a RepoWatchRepositoryState>,
    current: &'a RepoWatchRepositoryState,
    reaction_filter_unchanged: bool,
}

fn derive_pull_request_events(
    repository: &RepositorySlug,
    previous: Option<&RepoWatchPullRequestState>,
    current: &RepoWatchPullRequestState,
    repository_comparison: RepositoryComparison<'_>,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEvent>,
) -> Result<(), RepoWatchDifferError> {
    let context = current.context();
    let opened_now = current.lifecycle() == RepoWatchPullRequestLifecycle::Open
        && previous.is_none_or(|prior| prior.lifecycle() != RepoWatchPullRequestLifecycle::Open);
    match (
        previous.map(RepoWatchPullRequestState::lifecycle),
        current.lifecycle(),
    ) {
        (
            None
            | Some(RepoWatchPullRequestLifecycle::Closed | RepoWatchPullRequestLifecycle::Merged),
            RepoWatchPullRequestLifecycle::Open,
        ) => {
            push_pull_request_event(
                repository,
                context,
                RepoWatchEventKindV1::PullRequestOpened,
                ids,
                events,
            )?;
        }
        (Some(RepoWatchPullRequestLifecycle::Open), RepoWatchPullRequestLifecycle::Closed) => {
            push_pull_request_event(
                repository,
                context,
                RepoWatchEventKindV1::PullRequestClosed,
                ids,
                events,
            )?;
        }
        (
            Some(RepoWatchPullRequestLifecycle::Open | RepoWatchPullRequestLifecycle::Closed),
            RepoWatchPullRequestLifecycle::Merged,
        ) => {
            push_pull_request_event(
                repository,
                context,
                RepoWatchEventKindV1::PullRequestMerged,
                ids,
                events,
            )?;
        }
        (None, RepoWatchPullRequestLifecycle::Closed | RepoWatchPullRequestLifecycle::Merged)
        | (Some(RepoWatchPullRequestLifecycle::Open), RepoWatchPullRequestLifecycle::Open)
        | (Some(RepoWatchPullRequestLifecycle::Closed), RepoWatchPullRequestLifecycle::Closed)
        | (
            Some(RepoWatchPullRequestLifecycle::Merged),
            RepoWatchPullRequestLifecycle::Closed | RepoWatchPullRequestLifecycle::Merged,
        ) => {}
    }

    if opened_now {
        push_pull_request_event(
            repository,
            context,
            RepoWatchEventKindV1::MergeableStateChanged {
                current: current.mergeable_state(),
            },
            ids,
            events,
        )?;
        if previous.is_none() {
            return Ok(());
        }
    }
    let Some(previous) = previous else {
        return Ok(());
    };
    if previous.context().head_sha() != context.head_sha() {
        push_pull_request_event(
            repository,
            context,
            RepoWatchEventKindV1::HeadChanged {
                previous: previous.context().head_sha().clone(),
                current: context.head_sha().clone(),
            },
            ids,
            events,
        )?;
    }
    if previous.mergeable_state() != current.mergeable_state() {
        push_pull_request_event(
            repository,
            context,
            RepoWatchEventKindV1::MergeableStateChanged {
                current: current.mergeable_state(),
            },
            ids,
            events,
        )?;
    }
    derive_check_events(repository, previous, current, ids, events)?;
    derive_review_events(repository, previous, current, ids, events)?;
    derive_thread_events(repository, previous, current, ids, events)?;
    derive_label_events(repository, previous, current, ids, events)?;
    if let Some(previous_repository) = repository_comparison.previous {
        derive_base_advanced_event(
            repository,
            previous_repository,
            repository_comparison.current,
            current,
            ids,
            events,
        )?;
    }
    if repository_comparison.reaction_filter_unchanged {
        derive_reaction_events(repository, previous, current, ids, events)?;
    }
    Ok(())
}

fn derive_check_events(
    repository: &RepositorySlug,
    previous: &RepoWatchPullRequestState,
    current: &RepoWatchPullRequestState,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEvent>,
) -> Result<(), RepoWatchDifferError> {
    for suite in current.completed_check_suites() {
        if !previous
            .completed_check_suites()
            .iter()
            .any(|prior| prior.id() == suite.id())
        {
            push_pull_request_event(
                repository,
                current.context(),
                RepoWatchEventKindV1::ChecksCompleted {
                    outcome: suite.outcome(),
                },
                ids,
                events,
            )?;
        }
    }
    for run in current.completed_check_runs() {
        if !previous
            .completed_check_runs()
            .iter()
            .any(|prior| prior.id() == run.id())
        {
            push_pull_request_event(
                repository,
                current.context(),
                RepoWatchEventKindV1::CheckRunCompleted {
                    name: run.name().clone(),
                    conclusion: run.conclusion(),
                },
                ids,
                events,
            )?;
        }
    }
    Ok(())
}

fn derive_review_events(
    repository: &RepositorySlug,
    previous: &RepoWatchPullRequestState,
    current: &RepoWatchPullRequestState,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEvent>,
) -> Result<(), RepoWatchDifferError> {
    for review in current.reviews() {
        let newly_observed = !previous
            .reviews()
            .iter()
            .any(|prior| prior.id() == review.id());
        if newly_observed && let Some(state) = review.state() {
            push_pull_request_event(
                repository,
                current.context(),
                RepoWatchEventKindV1::ReviewSubmitted {
                    reviewer: review.reviewer().clone(),
                    state,
                    commit: review.commit().clone(),
                },
                ids,
                events,
            )?;
        }
    }
    Ok(())
}

fn derive_thread_events(
    repository: &RepositorySlug,
    previous: &RepoWatchPullRequestState,
    current: &RepoWatchPullRequestState,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEvent>,
) -> Result<(), RepoWatchDifferError> {
    for thread in current.threads() {
        let previous_state = previous
            .threads()
            .iter()
            .find(|prior| prior.thread() == thread.thread())
            .map(RepoWatchThreadObservation::state);
        if previous_state.is_none() {
            push_pull_request_event(
                repository,
                current.context(),
                RepoWatchEventKindV1::ThreadOpened {
                    thread: thread.thread().clone(),
                },
                ids,
                events,
            )?;
        }
        match (previous_state, thread.state()) {
            (None | Some(RepoWatchThreadState::Open), RepoWatchThreadState::Resolved) => {
                push_pull_request_event(
                    repository,
                    current.context(),
                    RepoWatchEventKindV1::ThreadResolved {
                        thread: thread.thread().clone(),
                    },
                    ids,
                    events,
                )?;
            }
            (Some(RepoWatchThreadState::Resolved), RepoWatchThreadState::Open) => {
                push_pull_request_event(
                    repository,
                    current.context(),
                    RepoWatchEventKindV1::ThreadOpened {
                        thread: thread.thread().clone(),
                    },
                    ids,
                    events,
                )?;
            }
            (None | Some(RepoWatchThreadState::Open), RepoWatchThreadState::Open)
            | (Some(RepoWatchThreadState::Resolved), RepoWatchThreadState::Resolved) => {}
        }
    }
    Ok(())
}

fn derive_label_events(
    repository: &RepositorySlug,
    previous: &RepoWatchPullRequestState,
    current: &RepoWatchPullRequestState,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEvent>,
) -> Result<(), RepoWatchDifferError> {
    for label in current.context().labels() {
        if !previous.context().labels().contains(label) {
            push_pull_request_event(
                repository,
                current.context(),
                RepoWatchEventKindV1::Labeled {
                    label: label.clone(),
                },
                ids,
                events,
            )?;
        }
    }
    for label in previous.context().labels() {
        if !current.context().labels().contains(label) {
            push_pull_request_event(
                repository,
                current.context(),
                RepoWatchEventKindV1::Unlabeled {
                    label: label.clone(),
                },
                ids,
                events,
            )?;
        }
    }
    Ok(())
}

fn derive_base_advanced_event(
    repository: &RepositorySlug,
    previous_repository: &RepoWatchRepositoryState,
    current_repository: &RepoWatchRepositoryState,
    current_pull_request: &RepoWatchPullRequestState,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEvent>,
) -> Result<(), RepoWatchDifferError> {
    if current_pull_request.lifecycle() != RepoWatchPullRequestLifecycle::Open {
        return Ok(());
    }
    let branch = current_pull_request.context().base_branch();
    let previous_head = find_branch_head(previous_repository.branch_heads(), branch);
    let current_head = find_branch_head(current_repository.branch_heads(), branch);
    if let (Some(previous_head), Some(current_head)) = (previous_head, current_head)
        && previous_head.head() != current_head.head()
    {
        push_pull_request_event(
            repository,
            current_pull_request.context(),
            RepoWatchEventKindV1::BaseAdvanced {
                branch: branch.clone(),
            },
            ids,
            events,
        )?;
    }
    Ok(())
}

fn derive_reaction_events(
    repository: &RepositorySlug,
    previous: &RepoWatchPullRequestState,
    current: &RepoWatchPullRequestState,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEvent>,
) -> Result<(), RepoWatchDifferError> {
    for reaction in current.reactions() {
        if !previous.reactions().contains(reaction) {
            push_reaction_event(
                repository,
                current,
                reaction,
                ReactionChange::Added,
                ids,
                events,
            )?;
        }
    }
    for reaction in previous.reactions() {
        if !current.reactions().contains(reaction) {
            push_reaction_event(
                repository,
                current,
                reaction,
                ReactionChange::Removed,
                ids,
                events,
            )?;
        }
    }
    Ok(())
}

fn push_reaction_event(
    repository: &RepositorySlug,
    current: &RepoWatchPullRequestState,
    reaction: &RepoWatchReactionObservation,
    change: ReactionChange,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEvent>,
) -> Result<(), RepoWatchDifferError> {
    push_pull_request_event(
        repository,
        current.context(),
        RepoWatchEventKindV1::ReactionChanged {
            subject: reaction.subject(),
            reactor: reaction.reactor().clone(),
            content: reaction.content().clone(),
            change,
        },
        ids,
        events,
    )
}

fn derive_workflow_events(
    repository: &RepositorySlug,
    previous: &RepoWatchRepositoryState,
    current: &RepoWatchRepositoryState,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEvent>,
) {
    for run in current.workflow_runs() {
        let already_observed = previous.workflow_runs().iter().any(|prior| {
            prior.branch() == run.branch()
                && prior.workflow() == run.workflow()
                && prior.id() == run.id()
        });
        if !already_observed {
            events.push(RepoWatchEvent::branch_workflow(
                ids.next_event_id(),
                repository.clone(),
                run.branch().clone(),
                run.workflow().clone(),
                run.conclusion(),
            ));
        }
    }
}

fn push_pull_request_event(
    repository: &RepositorySlug,
    context: &PullRequestEventContext,
    kind: RepoWatchEventKindV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEvent>,
) -> Result<(), RepoWatchDifferError> {
    let event = RepoWatchEvent::try_pull_request(
        ids.next_event_id(),
        repository.clone(),
        context.clone(),
        kind,
    )
    .map_err(RepoWatchDifferError)?;
    events.push(event);
    Ok(())
}

fn find_pull_request(
    pull_requests: &[RepoWatchPullRequestState],
    number: PullRequestNumber,
) -> Option<&RepoWatchPullRequestState> {
    pull_requests
        .binary_search_by_key(&number, |pull_request| pull_request.context().number())
        .ok()
        .map(|index| &pull_requests[index])
}

fn find_branch_head<'a>(
    branch_heads: &'a [RepoWatchBranchHead],
    branch: &BranchName,
) -> Option<&'a RepoWatchBranchHead> {
    branch_heads
        .binary_search_by(|candidate| candidate.branch().cmp(branch))
        .ok()
        .map(|index| &branch_heads[index])
}

fn reject_duplicate_object_ids<T>(
    values: &[T],
    identity: impl Fn(&T) -> GitHubObjectId,
    error: impl Fn(GitHubObjectId) -> RepoWatchRepositoryStateError,
) -> Result<(), RepoWatchRepositoryStateError> {
    for adjacent in values.windows(2) {
        if identity(&adjacent[0]) == identity(&adjacent[1]) {
            return Err(error(identity(&adjacent[0])));
        }
    }
    Ok(())
}

fn reject_duplicate_threads(
    threads: &[RepoWatchThreadObservation],
) -> Result<(), RepoWatchRepositoryStateError> {
    for adjacent in threads.windows(2) {
        if adjacent[0].thread() == adjacent[1].thread() {
            return Err(RepoWatchRepositoryStateError::DuplicateThread(
                adjacent[0].thread().clone(),
            ));
        }
    }
    Ok(())
}

fn reject_duplicate_pull_requests(
    pull_requests: &[RepoWatchPullRequestState],
) -> Result<(), RepoWatchRepositoryStateError> {
    for adjacent in pull_requests.windows(2) {
        if adjacent[0].context().number() == adjacent[1].context().number() {
            return Err(RepoWatchRepositoryStateError::DuplicatePullRequest(
                adjacent[0].context().number(),
            ));
        }
    }
    Ok(())
}

fn reject_duplicate_workflows(
    workflows: &[RepoWatchWorkflowRunObservation],
) -> Result<(), RepoWatchRepositoryStateError> {
    for adjacent in workflows.windows(2) {
        if adjacent[0].branch() == adjacent[1].branch()
            && adjacent[0].workflow() == adjacent[1].workflow()
        {
            return Err(RepoWatchRepositoryStateError::DuplicateWorkflow {
                branch: adjacent[0].branch().clone(),
                workflow: adjacent[0].workflow().clone(),
            });
        }
    }
    Ok(())
}

fn reject_duplicate_branch_heads(
    branch_heads: &[RepoWatchBranchHead],
) -> Result<(), RepoWatchRepositoryStateError> {
    for adjacent in branch_heads.windows(2) {
        if adjacent[0].branch() == adjacent[1].branch() {
            return Err(RepoWatchRepositoryStateError::DuplicateBranchHead(
                adjacent[0].branch().clone(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{error::Error, num::NonZeroU64};

    use signalbox_domain::{
        LabelName, PullRequestBody, PullRequestEventContextInput, PullRequestTitle,
        RepoWatchEventKindNameV1, RepoWatchTextError,
    };
    use uuid::Uuid;

    use super::*;

    const REPOSITORY: &str = "namespace/repo";
    const HEAD_REPOSITORY: &str = "contributor/repo";
    const BASE_BRANCH: &str = "main";
    const HEAD_BRANCH: &str = "topic/repo-watch";
    const TITLE: &str = "Add repository watch";
    const BODY: &str = "Typed repository events only.";
    const AUTHOR: &str = "maintainer";
    const REVIEWER: &str = "signal-reviewer";
    const INITIAL_HEAD: &str = "1111111111111111111111111111111111111111";
    const CHANGED_HEAD: &str = "2222222222222222222222222222222222222222";
    const INITIAL_BASE_HEAD: &str = "3333333333333333333333333333333333333333";
    const CHANGED_BASE_HEAD: &str = "4444444444444444444444444444444444444444";
    const REVIEW_COMMIT: &str = "5555555555555555555555555555555555555555";
    const CHECK_NAME: &str = "required";
    const WORKFLOW_NAME: &str = "continuous-integration";
    const THREAD_ID: &str = "PRRT_fixture";
    const LABEL_READY: &str = "ready";
    const LABEL_OLD: &str = "old";
    const REACTION_CONTENT: &str = "+1";
    const OTHER_REACTION_CONTENT: &str = "eyes";
    const PULL_REQUEST_NUMBER: u64 = 17;
    const OTHER_PULL_REQUEST_NUMBER: u64 = 3;
    const CHECK_SUITE_ID: u64 = 101;
    const CHECK_RUN_ID: u64 = 102;
    const REVIEW_ID: u64 = 103;
    const WORKFLOW_RUN_ID: u64 = 104;
    const NEXT_WORKFLOW_RUN_ID: u64 = 105;

    /// Deterministic event identities; their values are arbitrary and only their order matters.
    struct FixedEventIds {
        next: u128,
    }

    impl FixedEventIds {
        fn new() -> Self {
            Self { next: 1 }
        }
    }

    impl RepoWatchEventIdGenerator for FixedEventIds {
        fn next_event_id(&mut self) -> RepoWatchEventId {
            let id = RepoWatchEventId::from_uuid(Uuid::from_u128(self.next));
            self.next += 1;
            id
        }
    }

    /// Canonical pull-request fixture whose fields may be perturbed by name.
    struct PullRequestFacts {
        number: u64,
        lifecycle: RepoWatchPullRequestLifecycle,
        mergeable_state: MergeableState,
        head_sha: &'static str,
        labels: Vec<LabelName>,
        completed_check_suites: Vec<RepoWatchCheckSuiteObservation>,
        completed_check_runs: Vec<RepoWatchCheckRunObservation>,
        reviews: Vec<RepoWatchReviewObservation>,
        threads: Vec<RepoWatchThreadObservation>,
        reactions: Vec<RepoWatchReactionObservation>,
    }

    impl PullRequestFacts {
        fn matching(number: u64) -> Self {
            Self {
                number,
                lifecycle: RepoWatchPullRequestLifecycle::Open,
                mergeable_state: MergeableState::Mergeable,
                head_sha: INITIAL_HEAD,
                labels: Vec::new(),
                completed_check_suites: Vec::new(),
                completed_check_runs: Vec::new(),
                reviews: Vec::new(),
                threads: Vec::new(),
                reactions: Vec::new(),
            }
        }
    }

    fn repository() -> Result<RepositorySlug, RepoWatchTextError> {
        RepositorySlug::try_new(String::from(REPOSITORY))
    }

    fn object_id(value: u64) -> GitHubObjectId {
        GitHubObjectId::new(NonZeroU64::new(value).expect("fixture object identity is positive"))
    }

    fn pull_request_number(value: u64) -> PullRequestNumber {
        PullRequestNumber::new(
            NonZeroU64::new(value).expect("fixture pull-request number is positive"),
        )
    }

    fn label(value: &str) -> Result<LabelName, RepoWatchTextError> {
        LabelName::try_new(String::from(value))
    }

    fn reviewer(value: &str) -> Result<RepoWatchAuthorLogin, RepoWatchTextError> {
        RepoWatchAuthorLogin::try_new(String::from(value))
    }

    fn pull_request(facts: PullRequestFacts) -> Result<RepoWatchPullRequestState, Box<dyn Error>> {
        RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
            context: PullRequestEventContext::new(PullRequestEventContextInput {
                number: pull_request_number(facts.number),
                head_sha: CommitSha::try_new(String::from(facts.head_sha))?,
                head_repository: RepositorySlug::try_new(String::from(HEAD_REPOSITORY))?,
                base_branch: BranchName::try_new(String::from(BASE_BRANCH))?,
                head_branch: BranchName::try_new(String::from(HEAD_BRANCH))?,
                title: PullRequestTitle::try_new(String::from(TITLE))?,
                body: PullRequestBody::try_new(String::from(BODY))?,
                labels: facts.labels,
                draft: false,
                author: Some(RepoWatchAuthorLogin::try_new(String::from(AUTHOR))?),
            }),
            lifecycle: facts.lifecycle,
            mergeable_state: facts.mergeable_state,
            completed_check_suites: facts.completed_check_suites,
            completed_check_runs: facts.completed_check_runs,
            reviews: facts.reviews,
            threads: facts.threads,
            reactions: facts.reactions,
        })
        .map_err(Into::into)
    }

    fn branch_head(head: &str) -> Result<RepoWatchBranchHead, RepoWatchTextError> {
        Ok(RepoWatchBranchHead::new(
            BranchName::try_new(String::from(BASE_BRANCH))?,
            CommitSha::try_new(String::from(head))?,
        ))
    }

    fn workflow_run(
        id: u64,
        conclusion: CheckConclusion,
    ) -> Result<RepoWatchWorkflowRunObservation, RepoWatchTextError> {
        Ok(RepoWatchWorkflowRunObservation::new(
            object_id(id),
            BranchName::try_new(String::from(BASE_BRANCH))?,
            WorkflowName::try_new(String::from(WORKFLOW_NAME))?,
            conclusion,
        ))
    }

    fn observation(
        pull_requests: Vec<RepoWatchPullRequestState>,
        workflow_runs: Vec<RepoWatchWorkflowRunObservation>,
        branch_heads: Vec<RepoWatchBranchHead>,
        signal_reviewers: Vec<RepoWatchAuthorLogin>,
    ) -> Result<RepoWatchObservation, RepoWatchRepositoryStateError> {
        Ok(RepoWatchObservation::new(
            signal_reviewers,
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
                pull_requests,
                workflow_runs,
                branch_heads,
            })?,
        ))
    }

    fn derive(
        previous: Option<&RepoWatchObservation>,
        current: &RepoWatchObservation,
    ) -> Result<Vec<RepoWatchEvent>, Box<dyn Error>> {
        Ok(derive_repo_watch_events(
            &repository()?,
            previous,
            current,
            &mut FixedEventIds::new(),
        )?)
    }

    fn reaction() -> Result<RepoWatchReactionObservation, RepoWatchTextError> {
        Ok(RepoWatchReactionObservation::new(
            ReactionSubject::PullRequestBody,
            reviewer(REVIEWER)?,
            ReactionContent::try_new(String::from(REACTION_CONTENT))?,
        ))
    }

    #[test]
    fn initial_observation_emits_only_opened_and_current_mergeability() -> Result<(), Box<dyn Error>>
    {
        let current = observation(
            vec![pull_request(PullRequestFacts {
                completed_check_suites: vec![RepoWatchCheckSuiteObservation::new(
                    object_id(CHECK_SUITE_ID),
                    ChecksOutcome::Success,
                )],
                completed_check_runs: vec![RepoWatchCheckRunObservation::new(
                    object_id(CHECK_RUN_ID),
                    CheckRunName::try_new(String::from(CHECK_NAME))?,
                    CheckConclusion::Success,
                )],
                reviews: vec![RepoWatchReviewObservation::new(
                    object_id(REVIEW_ID),
                    reviewer(REVIEWER)?,
                    Some(ReviewState::Approved),
                    CommitSha::try_new(String::from(REVIEW_COMMIT))?,
                )],
                threads: vec![RepoWatchThreadObservation::new(
                    ReviewThreadId::try_new(String::from(THREAD_ID))?,
                    RepoWatchThreadState::Resolved,
                )],
                reactions: vec![reaction()?],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            vec![workflow_run(WORKFLOW_RUN_ID, CheckConclusion::Failure)?],
            vec![branch_head(INITIAL_BASE_HEAD)?],
            vec![reviewer(REVIEWER)?],
        )?;

        let events = derive(None, &current)?;

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind(), &RepoWatchEventKindV1::PullRequestOpened);
        assert_eq!(
            events[1].kind(),
            &RepoWatchEventKindV1::MergeableStateChanged {
                current: MergeableState::Mergeable,
            }
        );
        Ok(())
    }

    #[test]
    fn repeated_identical_observation_emits_nothing() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            vec![workflow_run(WORKFLOW_RUN_ID, CheckConclusion::Success)?],
            vec![branch_head(INITIAL_BASE_HEAD)?],
            vec![reviewer(REVIEWER)?],
        )?;
        let current = previous.clone();

        let events = derive(Some(&previous), &current)?;

        assert!(events.is_empty());
        Ok(())
    }

    #[test]
    fn merge_transition_emits_merged_without_closed() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let current = observation(
            vec![pull_request(PullRequestFacts {
                lifecycle: RepoWatchPullRequestLifecycle::Merged,
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind(), &RepoWatchEventKindV1::PullRequestMerged);
        Ok(())
    }

    #[test]
    fn close_transition_emits_closed() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let current = observation(
            vec![pull_request(PullRequestFacts {
                lifecycle: RepoWatchPullRequestLifecycle::Closed,
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind(), &RepoWatchEventKindV1::PullRequestClosed);
        Ok(())
    }

    #[test]
    fn reopened_pull_request_emits_opened_and_current_mergeability() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            vec![pull_request(PullRequestFacts {
                lifecycle: RepoWatchPullRequestLifecycle::Closed,
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let current = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind(), &RepoWatchEventKindV1::PullRequestOpened);
        assert_eq!(
            events[1].kind(),
            &RepoWatchEventKindV1::MergeableStateChanged {
                current: MergeableState::Mergeable,
            }
        );
        Ok(())
    }

    #[test]
    fn head_mergeability_and_completed_checks_emit_in_deterministic_order()
    -> Result<(), Box<dyn Error>> {
        let previous = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let current = observation(
            vec![pull_request(PullRequestFacts {
                head_sha: CHANGED_HEAD,
                mergeable_state: MergeableState::Conflicting,
                completed_check_suites: vec![RepoWatchCheckSuiteObservation::new(
                    object_id(CHECK_SUITE_ID),
                    ChecksOutcome::Failure,
                )],
                completed_check_runs: vec![RepoWatchCheckRunObservation::new(
                    object_id(CHECK_RUN_ID),
                    CheckRunName::try_new(String::from(CHECK_NAME))?,
                    CheckConclusion::TimedOut,
                )],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 4);
        assert_eq!(
            events[0].kind().name(),
            RepoWatchEventKindNameV1::HeadChanged
        );
        assert_eq!(
            events[1].kind(),
            &RepoWatchEventKindV1::MergeableStateChanged {
                current: MergeableState::Conflicting,
            }
        );
        assert_eq!(
            events[2].kind(),
            &RepoWatchEventKindV1::ChecksCompleted {
                outcome: ChecksOutcome::Failure,
            }
        );
        assert_eq!(
            events[3].kind(),
            &RepoWatchEventKindV1::CheckRunCompleted {
                name: CheckRunName::try_new(String::from(CHECK_NAME))?,
                conclusion: CheckConclusion::TimedOut,
            }
        );
        Ok(())
    }

    #[test]
    fn new_submitted_review_emits_review_fact() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let current_review = RepoWatchReviewObservation::new(
            object_id(REVIEW_ID),
            reviewer(REVIEWER)?,
            Some(ReviewState::ChangesRequested),
            CommitSha::try_new(String::from(REVIEW_COMMIT))?,
        );
        let current = observation(
            vec![pull_request(PullRequestFacts {
                reviews: vec![current_review.clone()],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].kind(),
            &RepoWatchEventKindV1::ReviewSubmitted {
                reviewer: current_review.reviewer().clone(),
                state: ReviewState::ChangesRequested,
                commit: current_review.commit().clone(),
            }
        );
        Ok(())
    }

    #[test]
    fn later_review_dismissal_emits_no_event() -> Result<(), Box<dyn Error>> {
        let submitted = RepoWatchReviewObservation::new(
            object_id(REVIEW_ID),
            reviewer(REVIEWER)?,
            Some(ReviewState::Approved),
            CommitSha::try_new(String::from(REVIEW_COMMIT))?,
        );
        let previous = observation(
            vec![pull_request(PullRequestFacts {
                reviews: vec![submitted],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let dismissed = RepoWatchReviewObservation::new(
            object_id(REVIEW_ID),
            reviewer(REVIEWER)?,
            None,
            CommitSha::try_new(String::from(REVIEW_COMMIT))?,
        );
        let current = observation(
            vec![pull_request(PullRequestFacts {
                reviews: vec![dismissed],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert!(events.is_empty());
        Ok(())
    }

    #[test]
    fn newly_observed_resolved_thread_emits_opened_then_resolved() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let thread = ReviewThreadId::try_new(String::from(THREAD_ID))?;
        let current = observation(
            vec![pull_request(PullRequestFacts {
                threads: vec![RepoWatchThreadObservation::new(
                    thread.clone(),
                    RepoWatchThreadState::Resolved,
                )],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].kind(),
            &RepoWatchEventKindV1::ThreadOpened {
                thread: thread.clone(),
            }
        );
        assert_eq!(
            events[1].kind(),
            &RepoWatchEventKindV1::ThreadResolved { thread }
        );
        Ok(())
    }

    #[test]
    fn labels_base_advance_and_reactions_emit_current_context_facts() -> Result<(), Box<dyn Error>>
    {
        let old_label = label(LABEL_OLD)?;
        let new_label = label(LABEL_READY)?;
        let previous_reaction = reaction()?;
        let previous = observation(
            vec![pull_request(PullRequestFacts {
                labels: vec![old_label.clone()],
                reactions: vec![previous_reaction.clone()],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            vec![branch_head(INITIAL_BASE_HEAD)?],
            vec![reviewer(REVIEWER)?],
        )?;
        let added_reaction = RepoWatchReactionObservation::new(
            ReactionSubject::IssueComment {
                id: object_id(REVIEW_ID),
            },
            reviewer(REVIEWER)?,
            ReactionContent::try_new(String::from(OTHER_REACTION_CONTENT))?,
        );
        let current = observation(
            vec![pull_request(PullRequestFacts {
                labels: vec![new_label.clone()],
                reactions: vec![added_reaction.clone()],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            vec![branch_head(CHANGED_BASE_HEAD)?],
            vec![reviewer(REVIEWER)?],
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 5);
        assert_eq!(
            events[0].kind(),
            &RepoWatchEventKindV1::Labeled { label: new_label }
        );
        assert_eq!(
            events[1].kind(),
            &RepoWatchEventKindV1::Unlabeled { label: old_label }
        );
        assert_eq!(
            events[2].kind(),
            &RepoWatchEventKindV1::BaseAdvanced {
                branch: BranchName::try_new(String::from(BASE_BRANCH))?,
            }
        );
        assert_eq!(
            events[3].kind(),
            &RepoWatchEventKindV1::ReactionChanged {
                subject: added_reaction.subject(),
                reactor: added_reaction.reactor().clone(),
                content: added_reaction.content().clone(),
                change: ReactionChange::Added,
            }
        );
        assert_eq!(
            events[4].kind(),
            &RepoWatchEventKindV1::ReactionChanged {
                subject: previous_reaction.subject(),
                reactor: previous_reaction.reactor().clone(),
                content: previous_reaction.content().clone(),
                change: ReactionChange::Removed,
            }
        );
        Ok(())
    }

    #[test]
    fn changed_signal_reviewer_set_rebaselines_only_reactions() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            vec![pull_request(PullRequestFacts {
                reactions: vec![reaction()?],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            vec![reviewer(REVIEWER)?],
        )?;
        let current = observation(
            vec![pull_request(PullRequestFacts {
                mergeable_state: MergeableState::Conflicting,
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            vec![reviewer("replacement-reviewer")?],
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].kind(),
            &RepoWatchEventKindV1::MergeableStateChanged {
                current: MergeableState::Conflicting,
            }
        );
        Ok(())
    }

    #[test]
    fn new_workflow_run_emits_even_when_conclusion_is_unchanged() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            Vec::new(),
            vec![workflow_run(WORKFLOW_RUN_ID, CheckConclusion::Failure)?],
            Vec::new(),
            Vec::new(),
        )?;
        let current_run = workflow_run(NEXT_WORKFLOW_RUN_ID, CheckConclusion::Failure)?;
        let current = observation(
            Vec::new(),
            vec![current_run.clone()],
            Vec::new(),
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].kind(),
            &RepoWatchEventKindV1::BranchWorkflowRunCompleted {
                branch: current_run.branch().clone(),
                workflow: current_run.workflow().clone(),
                conclusion: current_run.conclusion(),
            }
        );
        Ok(())
    }

    #[test]
    fn repository_state_sorts_pull_requests_by_number() -> Result<(), Box<dyn Error>> {
        let lower = pull_request(PullRequestFacts::matching(OTHER_PULL_REQUEST_NUMBER))?;
        let higher = pull_request(PullRequestFacts::matching(PULL_REQUEST_NUMBER))?;

        let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![higher, lower],
            workflow_runs: Vec::new(),
            branch_heads: Vec::new(),
        })?;

        assert_eq!(
            state.pull_requests()[0].context().number(),
            pull_request_number(OTHER_PULL_REQUEST_NUMBER)
        );
        assert_eq!(
            state.pull_requests()[1].context().number(),
            pull_request_number(PULL_REQUEST_NUMBER)
        );
        Ok(())
    }

    #[test]
    fn repository_state_rejects_duplicate_pull_request_numbers() -> Result<(), Box<dyn Error>> {
        let first = pull_request(PullRequestFacts::matching(PULL_REQUEST_NUMBER))?;
        let duplicate = pull_request(PullRequestFacts::matching(PULL_REQUEST_NUMBER))?;

        let result = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![first, duplicate],
            workflow_runs: Vec::new(),
            branch_heads: Vec::new(),
        });

        assert_eq!(
            result,
            Err(RepoWatchRepositoryStateError::DuplicatePullRequest(
                pull_request_number(PULL_REQUEST_NUMBER)
            ))
        );
        Ok(())
    }

    #[test]
    fn observation_canonicalizes_signal_reviewer_identity_set() -> Result<(), Box<dyn Error>> {
        let first = reviewer("z-reviewer")?;
        let second = reviewer("a-reviewer")?;

        let observation = RepoWatchObservation::new(
            vec![first.clone(), second.clone(), first],
            RepoWatchRepositoryState::default(),
        );

        assert_eq!(
            observation.signal_reviewers(),
            [second, reviewer("z-reviewer")?]
        );
        Ok(())
    }

    #[test]
    fn production_generator_supplies_distinct_uuid_v7_event_candidates() {
        let first = UuidV7RepoWatchEventIdGenerator.next_event_id();
        let second = UuidV7RepoWatchEventIdGenerator.next_event_id();

        assert_eq!(first.into_uuid().get_version_num(), 7);
        assert_eq!(second.into_uuid().get_version_num(), 7);
        assert_ne!(
            first, second,
            "successive event candidates must be distinct"
        );
    }
}
