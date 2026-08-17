//! Pure repository-state comparison for the repository-watch event boundary.

use std::{collections::BTreeSet, error::Error, fmt, future::Future};

use signalbox_domain::{
    AcceptedInputId, BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha,
    ContextFrontierId, CreateSession, DeliveryRequest, DurableCommandId, GitHubObjectId,
    GoalTextError, GoalUserAction, GoalUserCommand, MergeableState, ModelSelectionOverride,
    PerInputConfigurationChoices, PreparedCreateSession, PullRequestEventContext,
    PullRequestNumber, ReactionChange, ReactionContent, ReactionSubject, RepoWatchActionV1,
    RepoWatchAuthorLogin, RepoWatchDispatchContextError, RepoWatchDispatchId, RepoWatchEvent,
    RepoWatchEventConstructionError, RepoWatchEventId, RepoWatchEventKindV1, RepoWatchEventTarget,
    RepoWatchRule, RepoWatchRuleId, RepoWatchRuleVersion, RepoWatchSingletonScope,
    RepoWatchWorkflowRunAttempt, RepositorySlug, ReviewState, ReviewThreadId,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionCreationCause, SessionCreationProvenance, SessionId, SessionTemplateName,
    SessionTemplateProvenance, SubmitInput, TranscriptAncestry, TurnId, UserContent, WorkflowName,
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

// numeric-bound: tunable - admits the provider check-generation text this accepts
const MAX_CHECK_COMPLETION_GENERATION_BYTES: usize = 64;

/// Opaque provider generation for one completed check execution.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchCheckCompletionGeneration(String);

impl RepoWatchCheckCompletionGeneration {
    pub fn try_new(value: String) -> Result<Self, RepoWatchCheckCompletionGenerationError> {
        if value.is_empty()
            || value.len() > MAX_CHECK_COMPLETION_GENERATION_BYTES
            || value.contains('\0')
        {
            return Err(RepoWatchCheckCompletionGenerationError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Invalid provider check-completion generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchCheckCompletionGenerationError;

impl fmt::Display for RepoWatchCheckCompletionGenerationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "repository-watch check completion generation must contain 1 to {MAX_CHECK_COMPLETION_GENERATION_BYTES} NUL-free bytes"
        )
    }
}

impl Error for RepoWatchCheckCompletionGenerationError {}

/// One completed check-suite identity and its aggregate outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchCheckSuiteObservation {
    id: GitHubObjectId,
    completion_generation: RepoWatchCheckCompletionGeneration,
    outcome: ChecksOutcome,
}

impl RepoWatchCheckSuiteObservation {
    pub const fn new(
        id: GitHubObjectId,
        completion_generation: RepoWatchCheckCompletionGeneration,
        outcome: ChecksOutcome,
    ) -> Self {
        Self {
            id,
            completion_generation,
            outcome,
        }
    }

    pub const fn id(&self) -> GitHubObjectId {
        self.id
    }

    pub const fn completion_generation(&self) -> &RepoWatchCheckCompletionGeneration {
        &self.completion_generation
    }

    pub const fn outcome(&self) -> ChecksOutcome {
        self.outcome
    }
}

/// One completed check-run identity and its rule-visible result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchCheckRunObservation {
    id: GitHubObjectId,
    completion_generation: RepoWatchCheckCompletionGeneration,
    name: CheckRunName,
    conclusion: CheckConclusion,
}

impl RepoWatchCheckRunObservation {
    pub const fn new(
        id: GitHubObjectId,
        completion_generation: RepoWatchCheckCompletionGeneration,
        name: CheckRunName,
        conclusion: CheckConclusion,
    ) -> Self {
        Self {
            id,
            completion_generation,
            name,
            conclusion,
        }
    }

    pub const fn id(&self) -> GitHubObjectId {
        self.id
    }

    pub const fn completion_generation(&self) -> &RepoWatchCheckCompletionGeneration {
        &self.completion_generation
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

/// GitHub's aggregate review decision for one pull-request head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchReviewDecision {
    None,
    Approved,
    ReviewRequired,
    ChangesRequested,
}

/// Durable repository-watch convergence classification for one exact head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchConvergenceVerdict {
    NotConverged,
    InternallyConverged,
    MergeReady,
}

const MERGE_READY_BASE_BRANCH: &str = "main";

/// Field-labeled construction input for one exact-head convergence assessment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchConvergenceAssessmentInput {
    pub number: PullRequestNumber,
    pub head_sha: CommitSha,
    pub base_branch: BranchName,
    pub base_revision: CommitSha,
    pub mergeable_state: MergeableState,
    pub review_decision: RepoWatchReviewDecision,
    pub unresolved_threads: Vec<ReviewThreadId>,
    pub gating_check_count: u64,
    pub non_green_gating_checks: Vec<CheckRunName>,
}

/// Complete evidence and derived judgement for one exact pull-request head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchConvergenceAssessment {
    number: PullRequestNumber,
    head_sha: CommitSha,
    base_branch: BranchName,
    base_revision: CommitSha,
    mergeable_state: MergeableState,
    review_decision: RepoWatchReviewDecision,
    unresolved_threads: Box<[ReviewThreadId]>,
    gating_check_count: u64,
    non_green_gating_checks: Box<[CheckRunName]>,
    verdict: RepoWatchConvergenceVerdict,
}

impl RepoWatchConvergenceAssessment {
    /// Validates complete evidence and derives the reference convergence rule.
    pub fn try_new(
        mut input: RepoWatchConvergenceAssessmentInput,
    ) -> Result<Self, RepoWatchConvergenceAssessmentError> {
        input.unresolved_threads.sort();
        input.unresolved_threads.dedup();
        input
            .non_green_gating_checks
            .sort_by(|left, right| left.as_str().cmp(right.as_str()));
        if input.non_green_gating_checks.len() as u64 > input.gating_check_count {
            return Err(RepoWatchConvergenceAssessmentError);
        }
        let blocked = !input.unresolved_threads.is_empty()
            || !input.non_green_gating_checks.is_empty()
            || input.mergeable_state == MergeableState::Conflicting
            || input.review_decision == RepoWatchReviewDecision::ChangesRequested;
        let verdict = if blocked {
            RepoWatchConvergenceVerdict::NotConverged
        } else if input.base_branch.as_str() == MERGE_READY_BASE_BRANCH {
            RepoWatchConvergenceVerdict::MergeReady
        } else {
            RepoWatchConvergenceVerdict::InternallyConverged
        };
        Ok(Self {
            number: input.number,
            head_sha: input.head_sha,
            base_branch: input.base_branch,
            base_revision: input.base_revision,
            mergeable_state: input.mergeable_state,
            review_decision: input.review_decision,
            unresolved_threads: input.unresolved_threads.into_boxed_slice(),
            gating_check_count: input.gating_check_count,
            non_green_gating_checks: input.non_green_gating_checks.into_boxed_slice(),
            verdict,
        })
    }

    pub const fn number(&self) -> PullRequestNumber {
        self.number
    }

    pub const fn head_sha(&self) -> &CommitSha {
        &self.head_sha
    }

    pub const fn base_branch(&self) -> &BranchName {
        &self.base_branch
    }

    pub const fn base_revision(&self) -> &CommitSha {
        &self.base_revision
    }

    pub const fn mergeable_state(&self) -> MergeableState {
        self.mergeable_state
    }

    pub const fn review_decision(&self) -> RepoWatchReviewDecision {
        self.review_decision
    }

    pub fn unresolved_threads(&self) -> &[ReviewThreadId] {
        &self.unresolved_threads
    }

    pub const fn gating_check_count(&self) -> u64 {
        self.gating_check_count
    }

    pub fn non_green_gating_checks(&self) -> &[CheckRunName] {
        &self.non_green_gating_checks
    }

    pub const fn verdict(&self) -> RepoWatchConvergenceVerdict {
        self.verdict
    }
}

/// Incoherent convergence evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchConvergenceAssessmentError;

impl fmt::Display for RepoWatchConvergenceAssessmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "repository-watch convergence evidence is incoherent"
        )
    }
}

impl Error for RepoWatchConvergenceAssessmentError {}

/// One stale blocking review eligible for conservative automatic dismissal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchStaleReviewClearanceCandidate {
    number: PullRequestNumber,
    current_head_sha: CommitSha,
    review_node_id: Box<str>,
    reviewer: RepoWatchAuthorLogin,
    reviewed_head_sha: CommitSha,
}

impl RepoWatchStaleReviewClearanceCandidate {
    /// Requires the aggregate review decision to be the exact head's only
    /// remaining convergence blocker and the review to target an older head.
    pub fn try_new(
        assessment: &RepoWatchConvergenceAssessment,
        review_node_id: String,
        reviewer: RepoWatchAuthorLogin,
        reviewed_head_sha: CommitSha,
    ) -> Result<Self, RepoWatchStaleReviewClearanceCandidateError> {
        if assessment.review_decision() != RepoWatchReviewDecision::ChangesRequested
            || !assessment.unresolved_threads().is_empty()
            || !assessment.non_green_gating_checks().is_empty()
            || assessment.mergeable_state() == MergeableState::Conflicting
            || &reviewed_head_sha == assessment.head_sha()
            || review_node_id.is_empty()
            || review_node_id.len() > 256
            || review_node_id.contains('\0')
        {
            return Err(RepoWatchStaleReviewClearanceCandidateError);
        }
        Ok(Self {
            number: assessment.number(),
            current_head_sha: assessment.head_sha().clone(),
            review_node_id: review_node_id.into_boxed_str(),
            reviewer,
            reviewed_head_sha,
        })
    }

    pub const fn number(&self) -> PullRequestNumber {
        self.number
    }

    pub const fn current_head_sha(&self) -> &CommitSha {
        &self.current_head_sha
    }

    pub const fn review_node_id(&self) -> &str {
        &self.review_node_id
    }

    pub const fn reviewer(&self) -> &RepoWatchAuthorLogin {
        &self.reviewer
    }

    pub const fn reviewed_head_sha(&self) -> &CommitSha {
        &self.reviewed_head_sha
    }
}

/// A review is not stale, or another exact-head convergence blocker remains.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchStaleReviewClearanceCandidateError;

impl fmt::Display for RepoWatchStaleReviewClearanceCandidateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "repository-watch stale review clearance requires an older-head review and no blocker except changes requested",
        )
    }
}

impl Error for RepoWatchStaleReviewClearanceCandidateError {}

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
    workflow_id: GitHubObjectId,
    attempt: RepoWatchWorkflowRunAttempt,
    branch: BranchName,
    workflow: WorkflowName,
    conclusion: CheckConclusion,
}

impl RepoWatchWorkflowRunObservation {
    pub const fn new(
        id: GitHubObjectId,
        workflow_id: GitHubObjectId,
        attempt: RepoWatchWorkflowRunAttempt,
        branch: BranchName,
        workflow: WorkflowName,
        conclusion: CheckConclusion,
    ) -> Self {
        Self {
            id,
            workflow_id,
            attempt,
            branch,
            workflow,
            conclusion,
        }
    }

    pub const fn id(&self) -> GitHubObjectId {
        self.id
    }

    pub const fn workflow_id(&self) -> GitHubObjectId {
        self.workflow_id
    }

    pub const fn attempt(&self) -> RepoWatchWorkflowRunAttempt {
        self.attempt
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
        input
            .workflow_runs
            .sort_by_key(|run| (run.branch().clone(), run.workflow_id()));
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
        mut state: RepoWatchRepositoryState,
    ) -> Self {
        signal_reviewers.sort();
        signal_reviewers.dedup();
        for pull_request in &mut state.pull_requests {
            pull_request.reactions = pull_request
                .reactions
                .iter()
                .filter(|reaction| signal_reviewers.binary_search(reaction.reactor()).is_ok())
                .cloned()
                .collect::<Vec<_>>()
                .into_boxed_slice();
        }
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
        workflow_id: GitHubObjectId,
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
            Self::DuplicateWorkflow {
                branch,
                workflow_id,
            } => write!(
                formatter,
                "duplicate branch workflow {}/{}",
                branch.as_str(),
                workflow_id.get()
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
    }
    let Some(previous) = previous else {
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
    if !opened_now && previous.mergeable_state() != current.mergeable_state() {
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
        if !previous.completed_check_suites().iter().any(|prior| {
            prior.id() == suite.id()
                && prior.completion_generation() == suite.completion_generation()
        }) {
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
        if !previous.completed_check_runs().iter().any(|prior| {
            prior.id() == run.id()
                && prior.completion_generation() == run.completion_generation()
                && prior.conclusion() == run.conclusion()
        }) {
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
                && prior.id() == run.id()
                && prior.attempt() == run.attempt()
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
            && adjacent[0].workflow_id() == adjacent[1].workflow_id()
        {
            return Err(RepoWatchRepositoryStateError::DuplicateWorkflow {
                branch: adjacent[0].branch().clone(),
                workflow_id: adjacent[0].workflow_id(),
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

/// One completely resolved immutable session template used by dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchResolvedTemplate {
    provenance: SessionTemplateProvenance,
    defaults: SessionConfigurationDefaults,
}

impl RepoWatchResolvedTemplate {
    pub const fn new(
        provenance: SessionTemplateProvenance,
        defaults: SessionConfigurationDefaults,
    ) -> Self {
        Self {
            provenance,
            defaults,
        }
    }

    pub const fn provenance(&self) -> &SessionTemplateProvenance {
        &self.provenance
    }

    pub const fn defaults(&self) -> &SessionConfigurationDefaults {
        &self.defaults
    }
}

/// Immutable process-lifetime template lookup for repository-watch dispatch.
pub trait RepoWatchTemplateResolver {
    fn resolve_repo_watch_template(
        &self,
        name: &SessionTemplateName,
    ) -> Option<RepoWatchResolvedTemplate>;
}

/// Durable singleton identity derived independently for one matched rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchSingletonKey {
    PullRequest {
        repository: RepositorySlug,
        number: PullRequestNumber,
    },
    Stack {
        repository: RepositorySlug,
        root_pull_request: PullRequestNumber,
    },
    Rule,
    Repository {
        repository: RepositorySlug,
    },
}

/// One action whose current-interface session creation has been domain-prepared.
///
/// The turn reserved here is the only one a dispatched session receives. It
/// carries the tagged context, and the goal commissioned in the same
/// transaction adopts it as that generation's own first turn, so no separate
/// identity is reserved for the goal.
#[derive(Debug)]
pub struct RepoWatchPreparedDispatchAction {
    action: RepoWatchActionV1,
    prepared_session: PreparedCreateSession,
    initial_input: SubmitInput,
    accepted_input: AcceptedInputId,
    turn: TurnId,
    cancellation_entry: SemanticTranscriptEntryId,
    cancellation_frontier: ContextFrontierId,
    goal: GoalUserCommand,
}

impl RepoWatchPreparedDispatchAction {
    pub const fn action(&self) -> &RepoWatchActionV1 {
        &self.action
    }

    pub const fn prepared_session(&self) -> &PreparedCreateSession {
        &self.prepared_session
    }

    /// Returns the commission this dispatch composed for the created session.
    ///
    /// A dispatched session declares nothing about itself, so the goal that
    /// states its authority is composed here, from the dispatch, and committed
    /// with the session rather than left for the session to attach.
    pub const fn goal(&self) -> &GoalUserCommand {
        &self.goal
    }

    pub fn into_parts(
        self,
    ) -> (
        RepoWatchActionV1,
        PreparedCreateSession,
        SubmitInput,
        AcceptedInputId,
        TurnId,
        SemanticTranscriptEntryId,
        ContextFrontierId,
        GoalUserCommand,
    ) {
        (
            self.action,
            self.prepared_session,
            self.initial_input,
            self.accepted_input,
            self.turn,
            self.cancellation_entry,
            self.cancellation_frontier,
            self.goal,
        )
    }
}

/// One rule evaluation submitted to the atomic persistence boundary.
#[derive(Debug)]
pub enum RepoWatchRuleEvaluation {
    NotMatched {
        event: RepoWatchEvent,
        rule_id: RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
    },
    Matched {
        dispatch_id: RepoWatchDispatchId,
        event: RepoWatchEvent,
        rule_id: RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
        singleton: RepoWatchSingletonKey,
        cooldown: std::time::Duration,
        actions: Box<[RepoWatchPreparedDispatchAction]>,
    },
}

/// Result of one event/rule evaluation at the durability boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchRuleEvaluationOutcome {
    Inactive,
    NotMatched,
    TargetClosed,
    TargetConverged,
    Occupied,
    Cooldown,
    Dispatched {
        dispatch_id: RepoWatchDispatchId,
        sessions: Box<[SessionId]>,
    },
    Replayed {
        dispatch_id: RepoWatchDispatchId,
        sessions: Box<[SessionId]>,
    },
}

/// Atomic rule-evaluation, singleton-admission, session-creation, and audit port.
pub trait RepoWatchDispatchTransaction {
    type Error;

    fn handle_repo_watch_evaluation(
        &mut self,
        evaluation: RepoWatchRuleEvaluation,
    ) -> impl Future<Output = Result<RepoWatchRuleEvaluationOutcome, Self::Error>> + Send;
}

/// Candidate identity supply for one repository-watch dispatch batch.
pub trait RepoWatchDispatchIdGenerator {
    fn next_dispatch_id(&mut self) -> RepoWatchDispatchId;
    fn next_command_id(&mut self) -> DurableCommandId;
    fn next_session_id(&mut self) -> SessionId;
    fn next_accepted_input_id(&mut self) -> AcceptedInputId;
    fn next_turn_id(&mut self) -> TurnId;
    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_context_frontier_id(&mut self) -> ContextFrontierId;
}

/// Production UUIDv7 identity source for repository-watch dispatch.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7RepoWatchDispatchIdGenerator;

impl RepoWatchDispatchIdGenerator for UuidV7RepoWatchDispatchIdGenerator {
    fn next_dispatch_id(&mut self) -> RepoWatchDispatchId {
        RepoWatchDispatchId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_command_id(&mut self) -> DurableCommandId {
        DurableCommandId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_session_id(&mut self) -> SessionId {
        SessionId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_accepted_input_id(&mut self) -> AcceptedInputId {
        AcceptedInputId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_turn_id(&mut self) -> TurnId {
        TurnId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Why a validated rule could not be prepared for its atomic dispatch port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchDispatchPreparationError {
    Context(RepoWatchDispatchContextError),
    UnknownTemplate(SessionTemplateName),
    SessionPreparation,
    InvalidSingletonTarget,
    GoalStatement(GoalTextError),
}

impl fmt::Display for RepoWatchDispatchPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Context(_) => "repository-watch event could not form dispatch context",
            Self::UnknownTemplate(_) => "repository-watch rule names an unknown session template",
            Self::SessionPreparation => "repository-watch session preparation failed",
            Self::InvalidSingletonTarget => {
                "repository-watch singleton scope is incompatible with the event target"
            }
            Self::GoalStatement(_) => {
                "repository-watch dispatch could not form its synthesized goal statement"
            }
        })
    }
}

impl Error for RepoWatchDispatchPreparationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::GoalStatement(error) => Some(error),
            Self::UnknownTemplate(_) | Self::SessionPreparation | Self::InvalidSingletonTarget => {
                None
            }
        }
    }
}

/// Coordinates pure matching and current-interface session preparation.
#[derive(Debug)]
pub struct RepoWatchDispatchService<Ids, Transaction> {
    ids: Ids,
    transaction: Transaction,
}

impl<Ids, Transaction> RepoWatchDispatchService<Ids, Transaction> {
    pub const fn new(ids: Ids, transaction: Transaction) -> Self {
        Self { ids, transaction }
    }
}

impl<Ids, Transaction> RepoWatchDispatchService<Ids, Transaction>
where
    Ids: RepoWatchDispatchIdGenerator,
    Transaction: RepoWatchDispatchTransaction,
{
    pub async fn evaluate(
        &mut self,
        event: RepoWatchEvent,
        rule: &RepoWatchRule,
        observation: &RepoWatchObservation,
        templates: &impl RepoWatchTemplateResolver,
        context: UserContent,
    ) -> Result<RepoWatchRuleEvaluationOutcome, RepoWatchDispatchServiceError<Transaction::Error>>
    {
        let actions = rule
            .actions_for_event(&event)
            .map_err(RepoWatchDispatchPreparationError::Context)
            .map_err(RepoWatchDispatchServiceError::Preparation)?;
        if actions.is_empty() {
            return self
                .transaction
                .handle_repo_watch_evaluation(RepoWatchRuleEvaluation::NotMatched {
                    event,
                    rule_id: rule.id().clone(),
                    rule_version: rule.version(),
                })
                .await
                .map_err(RepoWatchDispatchServiceError::Transaction);
        }
        let singleton = singleton_key(rule.singleton_per(), &event, observation)
            .ok_or(RepoWatchDispatchPreparationError::InvalidSingletonTarget)
            .map_err(RepoWatchDispatchServiceError::Preparation)?;
        let mut prepared_actions = Vec::with_capacity(actions.len());
        for action in actions {
            let RepoWatchActionV1::DispatchSession(dispatch) = &action;
            let template = templates
                .resolve_repo_watch_template(dispatch.template())
                .ok_or_else(|| {
                    RepoWatchDispatchPreparationError::UnknownTemplate(dispatch.template().clone())
                })
                .map_err(RepoWatchDispatchServiceError::Preparation)?;
            let command = CreateSession::new_from_template(
                self.ids.next_command_id(),
                SessionCreationProvenance::new(
                    SessionCreationCause::UserInitiated,
                    TranscriptAncestry::None,
                ),
                template.provenance,
                template.defaults,
            );
            let prepared_session = command
                .prepare(self.ids.next_session_id())
                .map_err(|_| RepoWatchDispatchPreparationError::SessionPreparation)
                .map_err(RepoWatchDispatchServiceError::Preparation)?;
            let session = prepared_session.applied_result().session();
            let initial_input = SubmitInput::new(
                self.ids.next_command_id(),
                session,
                context.clone(),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            );
            let statement = dispatch
                .synthesized_goal_statement(rule.id())
                .map_err(RepoWatchDispatchPreparationError::GoalStatement)
                .map_err(RepoWatchDispatchServiceError::Preparation)?;
            let accepted_input = self.ids.next_accepted_input_id();
            let turn = self.ids.next_turn_id();
            let cancellation_entry = self.ids.next_semantic_entry_id();
            let cancellation_frontier = self.ids.next_context_frontier_id();
            let goal = GoalUserCommand::new(
                self.ids.next_command_id(),
                session,
                GoalUserAction::Attach(statement),
            );
            prepared_actions.push(RepoWatchPreparedDispatchAction {
                action,
                prepared_session,
                initial_input,
                accepted_input,
                turn,
                cancellation_entry,
                cancellation_frontier,
                goal,
            });
        }
        self.transaction
            .handle_repo_watch_evaluation(RepoWatchRuleEvaluation::Matched {
                dispatch_id: self.ids.next_dispatch_id(),
                event,
                rule_id: rule.id().clone(),
                rule_version: rule.version(),
                singleton,
                cooldown: rule.cooldown(),
                actions: prepared_actions.into_boxed_slice(),
            })
            .await
            .map_err(RepoWatchDispatchServiceError::Transaction)
    }
}

/// Nonterminal rule-dispatch orchestration failure.
#[derive(Debug)]
pub enum RepoWatchDispatchServiceError<TransactionError> {
    Preparation(RepoWatchDispatchPreparationError),
    Transaction(TransactionError),
}

fn singleton_key(
    scope: RepoWatchSingletonScope,
    event: &RepoWatchEvent,
    observation: &RepoWatchObservation,
) -> Option<RepoWatchSingletonKey> {
    match scope {
        RepoWatchSingletonScope::Rule => Some(RepoWatchSingletonKey::Rule),
        RepoWatchSingletonScope::Repository => Some(RepoWatchSingletonKey::Repository {
            repository: event.repository().clone(),
        }),
        RepoWatchSingletonScope::PullRequest => {
            let RepoWatchEventTarget::PullRequest(context) = event.target() else {
                return None;
            };
            Some(RepoWatchSingletonKey::PullRequest {
                repository: event.repository().clone(),
                number: context.number(),
            })
        }
        RepoWatchSingletonScope::Stack => {
            let RepoWatchEventTarget::PullRequest(context) = event.target() else {
                return None;
            };
            Some(RepoWatchSingletonKey::Stack {
                repository: event.repository().clone(),
                root_pull_request: stack_root_pull_request(
                    event.repository(),
                    context,
                    observation,
                ),
            })
        }
    }
}

fn stack_root_pull_request(
    repository: &RepositorySlug,
    context: &PullRequestEventContext,
    observation: &RepoWatchObservation,
) -> PullRequestNumber {
    let open_pull_requests = observation
        .state()
        .pull_requests()
        .iter()
        .filter(|pull_request| pull_request.lifecycle() == RepoWatchPullRequestLifecycle::Open)
        .collect::<Vec<_>>();
    let mut frontier = BTreeSet::from([context.number()]);
    let mut visited = BTreeSet::new();
    let mut component = BTreeSet::new();
    while let Some(number) = frontier.pop_first() {
        if !visited.insert(number) {
            continue;
        }
        let (candidate, candidate_is_open) = open_pull_requests
            .iter()
            .find(|pull_request| pull_request.context().number() == number)
            .map_or((context, false), |pull_request| {
                (pull_request.context(), true)
            });
        if candidate_is_open {
            component.insert(number);
        }
        frontier.extend(
            open_pull_requests
                .iter()
                .filter(|parent| {
                    parent.context().head_repository() == repository
                        && parent.context().head_branch() == candidate.base_branch()
                })
                .map(|parent| parent.context().number()),
        );
        if candidate_is_open && candidate.head_repository() == repository {
            frontier.extend(open_pull_requests.iter().filter_map(|child| {
                (child.context().base_branch() == candidate.head_branch())
                    .then_some(child.context().number())
            }));
        }
    }
    component
        .iter()
        .filter_map(|number| {
            open_pull_requests
                .iter()
                .find(|pull_request| pull_request.context().number() == *number)
                .map(|pull_request| (*number, pull_request.context()))
        })
        .find(|(_, candidate)| {
            !open_pull_requests.iter().any(|parent| {
                parent.context().head_repository() == repository
                    && parent.context().head_branch() == candidate.base_branch()
            })
        })
        .map(|(number, _)| number)
        .or_else(|| component.into_iter().next())
        .unwrap_or_else(|| context.number())
}

/// Display and error forwarding for repository-watch dispatch service failures.
impl<TransactionError> fmt::Display for RepoWatchDispatchServiceError<TransactionError>
where
    TransactionError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preparation(error) => error.fmt(formatter),
            Self::Transaction(error) => error.fmt(formatter),
        }
    }
}

impl<TransactionError> Error for RepoWatchDispatchServiceError<TransactionError> where
    TransactionError: Error + 'static
{
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
    const CHECK_COMPLETION_GENERATION: &str = "2026-08-02T12:00:00Z";
    const NEXT_CHECK_COMPLETION_GENERATION: &str = "2026-08-02T12:05:00Z";
    const NUL_CHECK_COMPLETION_GENERATION: &str = "generation\0";
    const OVERLONG_CHECK_COMPLETION_GENERATION: &str =
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    const WORKFLOW_NAME: &str = "continuous-integration";
    const RENAMED_WORKFLOW_NAME: &str = "continuous-integration-renamed";
    const THREAD_ID: &str = "PRRT_fixture";
    const LABEL_READY: &str = "ready";
    const LABEL_OLD: &str = "old";
    const REACTION_CONTENT: &str = "+1";
    const OTHER_REACTION_CONTENT: &str = "eyes";
    const PULL_REQUEST_NUMBER: u64 = 17;
    const OTHER_PULL_REQUEST_NUMBER: u64 = 3;
    const THIRD_PULL_REQUEST_NUMBER: u64 = 29;
    const FIRST_FORK_REPOSITORY: &str = "first/project";
    const SECOND_FORK_REPOSITORY: &str = "second/project";
    const OTHER_BASE_BRANCH: &str = "release";
    const FIRST_STACK_BRANCH: &str = "feature/first";
    const SECOND_STACK_BRANCH: &str = "feature/second";
    const SHARED_STACK_BRANCH: &str = "feature/shared";
    const BOTTOM_STACK_BRANCH: &str = "stack/bottom";
    const TOP_STACK_BRANCH: &str = "stack/top";
    const CHECK_SUITE_ID: u64 = 101;
    const CHECK_RUN_ID: u64 = 102;
    const REVIEW_ID: u64 = 103;
    const REVIEW_NODE_ID: &str = "review-node-103";
    const WORKFLOW_RUN_ID: u64 = 104;
    const NEXT_WORKFLOW_RUN_ID: u64 = 105;
    const WORKFLOW_ID: u64 = 106;
    const OTHER_WORKFLOW_ID: u64 = 107;
    const WORKFLOW_IDENTITIES: [u64; 2] = [WORKFLOW_ID, OTHER_WORKFLOW_ID];

    struct ConvergenceFacts {
        base_branch: &'static str,
        mergeable_state: MergeableState,
        review_decision: RepoWatchReviewDecision,
        unresolved_threads: Vec<ReviewThreadId>,
        gating_check_count: u64,
        non_green_gating_checks: Vec<CheckRunName>,
    }

    fn convergence_assessment(
        facts: ConvergenceFacts,
    ) -> Result<RepoWatchConvergenceAssessment, Box<dyn Error>> {
        Ok(RepoWatchConvergenceAssessment::try_new(
            RepoWatchConvergenceAssessmentInput {
                number: pull_request_number(PULL_REQUEST_NUMBER),
                head_sha: CommitSha::try_new(String::from(INITIAL_HEAD))?,
                base_branch: BranchName::try_new(String::from(facts.base_branch))?,
                base_revision: CommitSha::try_new(String::from(CHANGED_HEAD))?,
                mergeable_state: facts.mergeable_state,
                review_decision: facts.review_decision,
                unresolved_threads: facts.unresolved_threads,
                gating_check_count: facts.gating_check_count,
                non_green_gating_checks: facts.non_green_gating_checks,
            },
        )?)
    }

    #[test]
    fn exact_green_main_head_is_merge_ready_without_an_approval() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Unknown,
            review_decision: RepoWatchReviewDecision::None,
            unresolved_threads: Vec::new(),
            gating_check_count: 0,
            non_green_gating_checks: Vec::new(),
        })?;

        assert_eq!(
            assessment.verdict(),
            RepoWatchConvergenceVerdict::MergeReady
        );
        Ok(())
    }

    #[test]
    fn exact_green_stacked_head_is_only_internally_converged() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: FIRST_STACK_BRANCH,
            mergeable_state: MergeableState::Mergeable,
            review_decision: RepoWatchReviewDecision::Approved,
            unresolved_threads: Vec::new(),
            gating_check_count: 0,
            non_green_gating_checks: Vec::new(),
        })?;

        assert_eq!(
            assessment.verdict(),
            RepoWatchConvergenceVerdict::InternallyConverged
        );
        Ok(())
    }

    #[test]
    fn unresolved_thread_blocks_an_otherwise_green_head() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Mergeable,
            review_decision: RepoWatchReviewDecision::Approved,
            unresolved_threads: vec![ReviewThreadId::try_new(String::from(THREAD_ID))?],
            gating_check_count: 0,
            non_green_gating_checks: Vec::new(),
        })?;

        assert_eq!(
            assessment.verdict(),
            RepoWatchConvergenceVerdict::NotConverged
        );
        Ok(())
    }

    #[test]
    fn stale_blocking_review_blocks_an_otherwise_green_head() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Mergeable,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 0,
            non_green_gating_checks: Vec::new(),
        })?;

        assert_eq!(
            assessment.verdict(),
            RepoWatchConvergenceVerdict::NotConverged
        );
        Ok(())
    }

    #[test]
    fn older_head_review_is_clearable_when_it_is_the_only_blocker() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Mergeable,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        })?;

        let candidate = RepoWatchStaleReviewClearanceCandidate::try_new(
            &assessment,
            REVIEW_NODE_ID.to_owned(),
            RepoWatchAuthorLogin::try_new(String::from(REVIEWER))?,
            CommitSha::try_new(String::from(REVIEW_COMMIT))?,
        )?;

        assert_eq!(candidate.number(), assessment.number());
        assert_eq!(candidate.current_head_sha(), assessment.head_sha());
        assert_eq!(candidate.review_node_id(), REVIEW_NODE_ID);
        assert_eq!(candidate.reviewer().as_str(), REVIEWER);
        assert_eq!(candidate.reviewed_head_sha().as_str(), REVIEW_COMMIT);
        Ok(())
    }

    #[test]
    fn current_head_blocking_review_is_not_clearable() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Mergeable,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        })?;

        let result = RepoWatchStaleReviewClearanceCandidate::try_new(
            &assessment,
            REVIEW_NODE_ID.to_owned(),
            RepoWatchAuthorLogin::try_new(String::from(REVIEWER))?,
            assessment.head_sha().clone(),
        );

        assert_eq!(result, Err(RepoWatchStaleReviewClearanceCandidateError));
        Ok(())
    }

    #[test]
    fn unresolved_thread_prevents_stale_review_clearance() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Mergeable,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: vec![ReviewThreadId::try_new(String::from(THREAD_ID))?],
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        })?;

        let result = RepoWatchStaleReviewClearanceCandidate::try_new(
            &assessment,
            REVIEW_NODE_ID.to_owned(),
            RepoWatchAuthorLogin::try_new(String::from(REVIEWER))?,
            CommitSha::try_new(String::from(REVIEW_COMMIT))?,
        );

        assert_eq!(result, Err(RepoWatchStaleReviewClearanceCandidateError));
        Ok(())
    }

    #[test]
    fn non_green_check_prevents_stale_review_clearance() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Mergeable,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: vec![CheckRunName::try_new(String::from(CHECK_NAME))?],
        })?;

        let result = RepoWatchStaleReviewClearanceCandidate::try_new(
            &assessment,
            REVIEW_NODE_ID.to_owned(),
            RepoWatchAuthorLogin::try_new(String::from(REVIEWER))?,
            CommitSha::try_new(String::from(REVIEW_COMMIT))?,
        );

        assert_eq!(result, Err(RepoWatchStaleReviewClearanceCandidateError));
        Ok(())
    }

    #[test]
    fn merge_conflict_prevents_stale_review_clearance() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Conflicting,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        })?;

        let result = RepoWatchStaleReviewClearanceCandidate::try_new(
            &assessment,
            REVIEW_NODE_ID.to_owned(),
            RepoWatchAuthorLogin::try_new(String::from(REVIEWER))?,
            CommitSha::try_new(String::from(REVIEW_COMMIT))?,
        );

        assert_eq!(result, Err(RepoWatchStaleReviewClearanceCandidateError));
        Ok(())
    }

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

    fn completion_generation(
        value: &str,
    ) -> Result<RepoWatchCheckCompletionGeneration, RepoWatchCheckCompletionGenerationError> {
        RepoWatchCheckCompletionGeneration::try_new(String::from(value))
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
        workflow_run_for(id, WORKFLOW_ID, WORKFLOW_NAME, conclusion)
    }

    fn workflow_run_for(
        id: u64,
        workflow_id: u64,
        workflow_name: &str,
        conclusion: CheckConclusion,
    ) -> Result<RepoWatchWorkflowRunObservation, RepoWatchTextError> {
        workflow_run_for_attempt(id, workflow_id, workflow_name, 1, conclusion)
    }

    fn workflow_run_attempt(
        id: u64,
        attempt: u64,
        conclusion: CheckConclusion,
    ) -> Result<RepoWatchWorkflowRunObservation, RepoWatchTextError> {
        workflow_run_for_attempt(id, WORKFLOW_ID, WORKFLOW_NAME, attempt, conclusion)
    }

    fn workflow_run_for_attempt(
        id: u64,
        workflow_id: u64,
        workflow_name: &str,
        attempt: u64,
        conclusion: CheckConclusion,
    ) -> Result<RepoWatchWorkflowRunObservation, RepoWatchTextError> {
        Ok(RepoWatchWorkflowRunObservation::new(
            object_id(id),
            object_id(workflow_id),
            RepoWatchWorkflowRunAttempt::new(
                NonZeroU64::new(attempt).expect("fixture workflow attempt is positive"),
            ),
            BranchName::try_new(String::from(BASE_BRANCH))?,
            WorkflowName::try_new(String::from(workflow_name))?,
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

    fn stack_context(
        number: u64,
        base_branch: &str,
        head_branch: &str,
    ) -> Result<PullRequestEventContext, Box<dyn Error>> {
        stack_context_from(number, repository()?, base_branch, head_branch)
    }

    fn stack_context_from(
        number: u64,
        head_repository: RepositorySlug,
        base_branch: &str,
        head_branch: &str,
    ) -> Result<PullRequestEventContext, Box<dyn Error>> {
        Ok(PullRequestEventContext::new(PullRequestEventContextInput {
            number: pull_request_number(number),
            head_sha: CommitSha::try_new(String::from(INITIAL_HEAD))?,
            head_repository,
            base_branch: BranchName::try_new(String::from(base_branch))?,
            head_branch: BranchName::try_new(String::from(head_branch))?,
            title: PullRequestTitle::try_new(String::from(TITLE))?,
            body: PullRequestBody::try_new(String::from(BODY))?,
            labels: Vec::new(),
            draft: false,
            author: Some(RepoWatchAuthorLogin::try_new(String::from(AUTHOR))?),
        }))
    }

    fn stack_pull_request(
        context: PullRequestEventContext,
    ) -> Result<RepoWatchPullRequestState, Box<dyn Error>> {
        Ok(RepoWatchPullRequestState::try_new(
            RepoWatchPullRequestStateInput {
                context,
                lifecycle: RepoWatchPullRequestLifecycle::Open,
                mergeable_state: MergeableState::Mergeable,
                completed_check_suites: Vec::new(),
                completed_check_runs: Vec::new(),
                reviews: Vec::new(),
                threads: Vec::new(),
                reactions: Vec::new(),
            },
        )?)
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

    #[test]
    fn independent_pull_requests_to_one_base_have_distinct_stack_root_pull_requests()
    -> Result<(), Box<dyn Error>> {
        let first = stack_context(PULL_REQUEST_NUMBER, BASE_BRANCH, FIRST_STACK_BRANCH)?;
        let second = stack_context(OTHER_PULL_REQUEST_NUMBER, BASE_BRANCH, SECOND_STACK_BRANCH)?;
        let state = observation(
            vec![
                stack_pull_request(first.clone())?,
                stack_pull_request(second.clone())?,
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        assert_eq!(
            stack_root_pull_request(&repository()?, &first, &state),
            first.number()
        );
        assert_eq!(
            stack_root_pull_request(&repository()?, &second, &state),
            second.number()
        );
        Ok(())
    }

    #[test]
    fn fork_pull_requests_with_equal_head_branch_have_distinct_stack_root_pull_requests()
    -> Result<(), Box<dyn Error>> {
        let first = stack_context_from(
            PULL_REQUEST_NUMBER,
            RepositorySlug::try_new(String::from(FIRST_FORK_REPOSITORY))?,
            BASE_BRANCH,
            SHARED_STACK_BRANCH,
        )?;
        let second = stack_context_from(
            OTHER_PULL_REQUEST_NUMBER,
            RepositorySlug::try_new(String::from(SECOND_FORK_REPOSITORY))?,
            BASE_BRANCH,
            SHARED_STACK_BRANCH,
        )?;
        let state = observation(
            vec![
                stack_pull_request(first.clone())?,
                stack_pull_request(second.clone())?,
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        assert_eq!(
            stack_root_pull_request(&repository()?, &first, &state),
            first.number()
        );
        assert_eq!(
            stack_root_pull_request(&repository()?, &second, &state),
            second.number()
        );
        Ok(())
    }

    #[test]
    fn chained_pull_requests_share_the_bottom_pull_request_as_stack_root()
    -> Result<(), Box<dyn Error>> {
        let bottom = stack_context(PULL_REQUEST_NUMBER, BASE_BRANCH, BOTTOM_STACK_BRANCH)?;
        let top = stack_context(
            OTHER_PULL_REQUEST_NUMBER,
            BOTTOM_STACK_BRANCH,
            TOP_STACK_BRANCH,
        )?;
        let state = observation(
            vec![
                stack_pull_request(bottom.clone())?,
                stack_pull_request(top.clone())?,
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        assert_eq!(
            stack_root_pull_request(&repository()?, &top, &state),
            bottom.number()
        );
        Ok(())
    }

    #[test]
    fn branching_pull_requests_share_one_canonical_stack_root() -> Result<(), Box<dyn Error>> {
        let first_parent = stack_context(PULL_REQUEST_NUMBER, BASE_BRANCH, SHARED_STACK_BRANCH)?;
        let second_parent = stack_context(
            OTHER_PULL_REQUEST_NUMBER,
            OTHER_BASE_BRANCH,
            SHARED_STACK_BRANCH,
        )?;
        let child = stack_context(
            THIRD_PULL_REQUEST_NUMBER,
            SHARED_STACK_BRANCH,
            TOP_STACK_BRANCH,
        )?;
        let state = observation(
            vec![
                stack_pull_request(first_parent.clone())?,
                stack_pull_request(second_parent.clone())?,
                stack_pull_request(child.clone())?,
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        assert_eq!(
            stack_root_pull_request(&repository()?, &first_parent, &state),
            second_parent.number()
        );
        assert_eq!(
            stack_root_pull_request(&repository()?, &second_parent, &state),
            second_parent.number()
        );
        assert_eq!(
            stack_root_pull_request(&repository()?, &child, &state),
            second_parent.number()
        );
        Ok(())
    }

    #[test]
    fn cyclic_pull_requests_share_one_canonical_stack_root() -> Result<(), Box<dyn Error>> {
        let first = stack_context(PULL_REQUEST_NUMBER, SECOND_STACK_BRANCH, FIRST_STACK_BRANCH)?;
        let second = stack_context(
            OTHER_PULL_REQUEST_NUMBER,
            FIRST_STACK_BRANCH,
            SECOND_STACK_BRANCH,
        )?;
        let state = observation(
            vec![
                stack_pull_request(first.clone())?,
                stack_pull_request(second.clone())?,
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        assert_eq!(
            stack_root_pull_request(&repository()?, &first, &state),
            second.number()
        );
        assert_eq!(
            stack_root_pull_request(&repository()?, &second, &state),
            second.number()
        );
        Ok(())
    }

    fn reaction() -> Result<RepoWatchReactionObservation, RepoWatchTextError> {
        Ok(RepoWatchReactionObservation::new(
            ReactionSubject::PullRequestBody,
            reviewer(REVIEWER)?,
            ReactionContent::try_new(String::from(REACTION_CONTENT))?,
        ))
    }

    #[test]
    fn empty_check_completion_generation_is_rejected() {
        let result = RepoWatchCheckCompletionGeneration::try_new(String::new());

        assert_eq!(result, Err(RepoWatchCheckCompletionGenerationError));
    }

    #[test]
    fn overlong_check_completion_generation_is_rejected() {
        let result = RepoWatchCheckCompletionGeneration::try_new(String::from(
            OVERLONG_CHECK_COMPLETION_GENERATION,
        ));

        assert_eq!(result, Err(RepoWatchCheckCompletionGenerationError));
    }

    #[test]
    fn nul_check_completion_generation_is_rejected() {
        let result = RepoWatchCheckCompletionGeneration::try_new(String::from(
            NUL_CHECK_COMPLETION_GENERATION,
        ));

        assert_eq!(result, Err(RepoWatchCheckCompletionGenerationError));
    }

    #[test]
    fn initial_observation_emits_only_opened_and_current_mergeability() -> Result<(), Box<dyn Error>>
    {
        let current = observation(
            vec![pull_request(PullRequestFacts {
                completed_check_suites: vec![RepoWatchCheckSuiteObservation::new(
                    object_id(CHECK_SUITE_ID),
                    completion_generation(CHECK_COMPLETION_GENERATION)?,
                    ChecksOutcome::Success,
                )],
                completed_check_runs: vec![RepoWatchCheckRunObservation::new(
                    object_id(CHECK_RUN_ID),
                    completion_generation(CHECK_COMPLETION_GENERATION)?,
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
                mergeable_state: MergeableState::Unknown,
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
        let current_check_run = RepoWatchCheckRunObservation::new(
            object_id(CHECK_RUN_ID),
            completion_generation(CHECK_COMPLETION_GENERATION)?,
            CheckRunName::try_new(String::from(CHECK_NAME))?,
            CheckConclusion::TimedOut,
        );
        let current = observation(
            vec![pull_request(PullRequestFacts {
                head_sha: CHANGED_HEAD,
                mergeable_state: MergeableState::Conflicting,
                completed_check_suites: vec![RepoWatchCheckSuiteObservation::new(
                    object_id(CHECK_SUITE_ID),
                    completion_generation(CHECK_COMPLETION_GENERATION)?,
                    ChecksOutcome::Failure,
                )],
                completed_check_runs: vec![current_check_run.clone()],
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
                name: current_check_run.name().clone(),
                conclusion: current_check_run.conclusion(),
            }
        );
        Ok(())
    }

    #[test]
    fn recompleted_check_suite_with_same_provider_id_emits_again() -> Result<(), Box<dyn Error>> {
        let previous_suite = RepoWatchCheckSuiteObservation::new(
            object_id(CHECK_SUITE_ID),
            completion_generation(CHECK_COMPLETION_GENERATION)?,
            ChecksOutcome::Success,
        );
        let previous = observation(
            vec![pull_request(PullRequestFacts {
                completed_check_suites: vec![previous_suite],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let current_suite = RepoWatchCheckSuiteObservation::new(
            object_id(CHECK_SUITE_ID),
            completion_generation(NEXT_CHECK_COMPLETION_GENERATION)?,
            ChecksOutcome::Success,
        );
        let current = observation(
            vec![pull_request(PullRequestFacts {
                completed_check_suites: vec![current_suite.clone()],
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
            &RepoWatchEventKindV1::ChecksCompleted {
                outcome: current_suite.outcome(),
            }
        );
        Ok(())
    }

    #[test]
    fn recompleted_check_run_with_same_provider_id_emits_again() -> Result<(), Box<dyn Error>> {
        let previous_run = RepoWatchCheckRunObservation::new(
            object_id(CHECK_RUN_ID),
            completion_generation(CHECK_COMPLETION_GENERATION)?,
            CheckRunName::try_new(String::from(CHECK_NAME))?,
            CheckConclusion::Success,
        );
        let previous = observation(
            vec![pull_request(PullRequestFacts {
                completed_check_runs: vec![previous_run],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let current_run = RepoWatchCheckRunObservation::new(
            object_id(CHECK_RUN_ID),
            completion_generation(NEXT_CHECK_COMPLETION_GENERATION)?,
            CheckRunName::try_new(String::from(CHECK_NAME))?,
            CheckConclusion::Success,
        );
        let current = observation(
            vec![pull_request(PullRequestFacts {
                completed_check_runs: vec![current_run.clone()],
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
            &RepoWatchEventKindV1::CheckRunCompleted {
                name: current_run.name().clone(),
                conclusion: current_run.conclusion(),
            }
        );
        Ok(())
    }

    #[test]
    fn edited_check_run_conclusion_emits_again_under_one_generation() -> Result<(), Box<dyn Error>>
    {
        let previous_run = RepoWatchCheckRunObservation::new(
            object_id(CHECK_RUN_ID),
            completion_generation(CHECK_COMPLETION_GENERATION)?,
            CheckRunName::try_new(String::from(CHECK_NAME))?,
            CheckConclusion::Success,
        );
        let previous = observation(
            vec![pull_request(PullRequestFacts {
                completed_check_runs: vec![previous_run],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let current_run = RepoWatchCheckRunObservation::new(
            object_id(CHECK_RUN_ID),
            completion_generation(CHECK_COMPLETION_GENERATION)?,
            CheckRunName::try_new(String::from(CHECK_NAME))?,
            CheckConclusion::Failure,
        );
        let current = observation(
            vec![pull_request(PullRequestFacts {
                completed_check_runs: vec![current_run.clone()],
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
            &RepoWatchEventKindV1::CheckRunCompleted {
                name: current_run.name().clone(),
                conclusion: current_run.conclusion(),
            }
        );
        Ok(())
    }

    #[test]
    fn unchanged_check_run_emits_no_event() -> Result<(), Box<dyn Error>> {
        let run = RepoWatchCheckRunObservation::new(
            object_id(CHECK_RUN_ID),
            completion_generation(CHECK_COMPLETION_GENERATION)?,
            CheckRunName::try_new(String::from(CHECK_NAME))?,
            CheckConclusion::Success,
        );
        let previous = observation(
            vec![pull_request(PullRequestFacts {
                completed_check_runs: vec![run.clone()],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let current = observation(
            vec![pull_request(PullRequestFacts {
                completed_check_runs: vec![run],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        assert!(derive(Some(&previous), &current)?.is_empty());
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
    fn label_changes_emit_current_context_facts() -> Result<(), Box<dyn Error>> {
        let old_label = label(LABEL_OLD)?;
        let new_label = label(LABEL_READY)?;
        let previous = observation(
            vec![pull_request(PullRequestFacts {
                labels: vec![old_label.clone()],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let current = observation(
            vec![pull_request(PullRequestFacts {
                labels: vec![new_label.clone()],
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
            &RepoWatchEventKindV1::Labeled { label: new_label }
        );
        assert_eq!(
            events[1].kind(),
            &RepoWatchEventKindV1::Unlabeled { label: old_label }
        );
        Ok(())
    }

    #[test]
    fn base_advance_emits_current_context_fact() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            Vec::new(),
            vec![branch_head(INITIAL_BASE_HEAD)?],
            Vec::new(),
        )?;
        let current_pull_request = pull_request(PullRequestFacts::matching(PULL_REQUEST_NUMBER))?;
        let expected_branch = current_pull_request.context().base_branch().clone();
        let current = observation(
            vec![current_pull_request],
            Vec::new(),
            vec![branch_head(CHANGED_BASE_HEAD)?],
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].kind(),
            &RepoWatchEventKindV1::BaseAdvanced {
                branch: expected_branch,
            }
        );
        Ok(())
    }

    #[test]
    fn newly_observed_pull_request_emits_base_advance() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            Vec::new(),
            Vec::new(),
            vec![branch_head(INITIAL_BASE_HEAD)?],
            Vec::new(),
        )?;
        let current_pull_request = pull_request(PullRequestFacts::matching(PULL_REQUEST_NUMBER))?;
        let expected_branch = current_pull_request.context().base_branch().clone();
        let current = observation(
            vec![current_pull_request],
            Vec::new(),
            vec![branch_head(CHANGED_BASE_HEAD)?],
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind(), &RepoWatchEventKindV1::PullRequestOpened);
        assert_eq!(
            events[1].kind(),
            &RepoWatchEventKindV1::MergeableStateChanged {
                current: MergeableState::Mergeable,
            }
        );
        assert_eq!(
            events[2].kind(),
            &RepoWatchEventKindV1::BaseAdvanced {
                branch: expected_branch,
            }
        );
        Ok(())
    }

    #[test]
    fn reaction_changes_emit_current_context_facts() -> Result<(), Box<dyn Error>> {
        let previous_reaction = reaction()?;
        let previous = observation(
            vec![pull_request(PullRequestFacts {
                reactions: vec![previous_reaction.clone()],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
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
                reactions: vec![added_reaction.clone()],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            vec![reviewer(REVIEWER)?],
        )?;

        let events = derive(Some(&previous), &current)?;

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].kind(),
            &RepoWatchEventKindV1::ReactionChanged {
                subject: added_reaction.subject(),
                reactor: added_reaction.reactor().clone(),
                content: added_reaction.content().clone(),
                change: ReactionChange::Added,
            }
        );
        assert_eq!(
            events[1].kind(),
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
    fn workflow_rename_does_not_reemit_an_existing_run() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            Vec::new(),
            vec![workflow_run_for(
                WORKFLOW_RUN_ID,
                WORKFLOW_ID,
                WORKFLOW_NAME,
                CheckConclusion::Success,
            )?],
            Vec::new(),
            Vec::new(),
        )?;
        let current = observation(
            Vec::new(),
            vec![workflow_run_for(
                WORKFLOW_RUN_ID,
                WORKFLOW_ID,
                RENAMED_WORKFLOW_NAME,
                CheckConclusion::Success,
            )?],
            Vec::new(),
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert!(events.is_empty());
        Ok(())
    }

    #[test]
    fn legacy_workflow_identity_upgrade_does_not_reemit_an_existing_run()
    -> Result<(), Box<dyn Error>> {
        let previous = observation(
            Vec::new(),
            vec![workflow_run_for(
                WORKFLOW_RUN_ID,
                WORKFLOW_RUN_ID,
                WORKFLOW_NAME,
                CheckConclusion::Success,
            )?],
            Vec::new(),
            Vec::new(),
        )?;
        let current = observation(
            Vec::new(),
            vec![workflow_run_for(
                WORKFLOW_RUN_ID,
                WORKFLOW_ID,
                WORKFLOW_NAME,
                CheckConclusion::Success,
            )?],
            Vec::new(),
            Vec::new(),
        )?;

        let events = derive(Some(&previous), &current)?;

        assert!(events.is_empty());
        Ok(())
    }

    #[test]
    fn repository_state_accepts_duplicate_workflow_names_with_distinct_identities()
    -> Result<(), Box<dyn Error>> {
        let first = workflow_run_for(
            WORKFLOW_RUN_ID,
            WORKFLOW_ID,
            WORKFLOW_NAME,
            CheckConclusion::Success,
        )?;
        let second = workflow_run_for(
            NEXT_WORKFLOW_RUN_ID,
            OTHER_WORKFLOW_ID,
            WORKFLOW_NAME,
            CheckConclusion::Failure,
        )?;

        let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: Vec::new(),
            workflow_runs: vec![second, first],
            branch_heads: Vec::new(),
        })?;

        assert_eq!(state.workflow_runs().len(), WORKFLOW_IDENTITIES.len());
        assert_eq!(
            state.workflow_runs()[0].workflow_id(),
            object_id(WORKFLOW_ID)
        );
        assert_eq!(
            state.workflow_runs()[1].workflow_id(),
            object_id(OTHER_WORKFLOW_ID)
        );
        Ok(())
    }

    #[test]
    fn repository_state_rejects_duplicate_branch_workflow_identities() -> Result<(), Box<dyn Error>>
    {
        let first = workflow_run_for(
            WORKFLOW_RUN_ID,
            WORKFLOW_ID,
            WORKFLOW_NAME,
            CheckConclusion::Success,
        )?;
        let duplicate = workflow_run_for(
            NEXT_WORKFLOW_RUN_ID,
            WORKFLOW_ID,
            RENAMED_WORKFLOW_NAME,
            CheckConclusion::Failure,
        )?;

        let result = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: Vec::new(),
            workflow_runs: vec![first, duplicate],
            branch_heads: Vec::new(),
        });

        assert_eq!(
            result,
            Err(RepoWatchRepositoryStateError::DuplicateWorkflow {
                branch: BranchName::try_new(String::from(BASE_BRANCH))?,
                workflow_id: object_id(WORKFLOW_ID),
            })
        );
        Ok(())
    }

    #[test]
    fn new_workflow_attempt_emits_when_run_identity_is_unchanged() -> Result<(), Box<dyn Error>> {
        let previous = observation(
            Vec::new(),
            vec![workflow_run_attempt(
                WORKFLOW_RUN_ID,
                1,
                CheckConclusion::Failure,
            )?],
            Vec::new(),
            Vec::new(),
        )?;
        let current_run = workflow_run_attempt(WORKFLOW_RUN_ID, 2, CheckConclusion::Success)?;
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
    fn observation_excludes_reactions_from_non_signal_reviewers() -> Result<(), Box<dyn Error>> {
        let observation = observation(
            vec![pull_request(PullRequestFacts {
                reactions: vec![reaction()?],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;

        assert!(
            observation.state().pull_requests()[0]
                .reactions()
                .is_empty()
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
