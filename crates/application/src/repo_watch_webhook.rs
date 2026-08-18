//! Transport-independent GitHub webhook payload projection for repository watch.

use std::{collections::BTreeSet, error::Error, fmt, num::NonZeroU64};

use serde_json::{Map, Value};
use signalbox_domain::{
    BranchName, CheckConclusion, CheckRunName, CommitSha, GitHubObjectId, LabelName,
    MergeableState, PullRequestBody, PullRequestEventContext, PullRequestEventContextInput,
    PullRequestNumber, PullRequestTitle, RepoWatchAuthorLogin, RepoWatchWorkflowRunAttempt,
    RepositorySlug, ReviewState, ReviewThreadId, WorkflowName,
};
use uuid::Uuid;

use crate::{
    RepoWatchBranchHead, RepoWatchCheckCompletionGeneration, RepoWatchCheckRunObservation,
    RepoWatchObservation, RepoWatchPullRequestLifecycle, RepoWatchPullRequestState,
    RepoWatchPullRequestStateInput, RepoWatchRepositoryState, RepoWatchRepositoryStateError,
    RepoWatchRepositoryStateInput, RepoWatchReviewObservation, RepoWatchThreadObservation,
    RepoWatchThreadState, RepoWatchWorkflowRunObservation,
};

/// Durable coordinates for the exact admitted body consumed by the mapper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookBodyReferenceV1 {
    hook_id: NonZeroU64,
    delivery_id: Uuid,
}

impl RepoWatchWebhookBodyReferenceV1 {
    pub const fn new(hook_id: NonZeroU64, delivery_id: Uuid) -> Self {
        Self {
            hook_id,
            delivery_id,
        }
    }

    pub const fn hook_id(&self) -> NonZeroU64 {
        self.hook_id
    }

    pub const fn delivery_id(&self) -> Uuid {
        self.delivery_id
    }
}

/// Field-labeled metadata for one authenticated and durably admitted delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookDeliveryV1Input {
    pub repository: RepositorySlug,
    pub hook_id: NonZeroU64,
    pub delivery_id: Uuid,
    pub event: String,
    pub action: Option<String>,
    pub receipt_sequence: NonZeroU64,
    pub body_digest: [u8; 32],
}

/// Transport-independent metadata for one authenticated GitHub delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookDeliveryV1 {
    repository: RepositorySlug,
    hook_id: NonZeroU64,
    delivery_id: Uuid,
    event: String,
    action: Option<String>,
    receipt_sequence: NonZeroU64,
    body_digest: [u8; 32],
    body_reference: RepoWatchWebhookBodyReferenceV1,
}

impl RepoWatchWebhookDeliveryV1 {
    pub fn new(input: RepoWatchWebhookDeliveryV1Input) -> Self {
        Self {
            repository: input.repository,
            hook_id: input.hook_id,
            delivery_id: input.delivery_id,
            event: input.event,
            action: input.action,
            receipt_sequence: input.receipt_sequence,
            body_digest: input.body_digest,
            body_reference: RepoWatchWebhookBodyReferenceV1::new(input.hook_id, input.delivery_id),
        }
    }

    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }

    pub const fn hook_id(&self) -> NonZeroU64 {
        self.hook_id
    }

    pub const fn delivery_id(&self) -> Uuid {
        self.delivery_id
    }

    pub fn event(&self) -> &str {
        &self.event
    }

    pub fn action(&self) -> Option<&str> {
        self.action.as_deref()
    }

    pub const fn receipt_sequence(&self) -> NonZeroU64 {
        self.receipt_sequence
    }

    pub const fn body_digest(&self) -> &[u8; 32] {
        &self.body_digest
    }

    pub const fn body_reference(&self) -> RepoWatchWebhookBodyReferenceV1 {
        self.body_reference
    }
}

/// What to do if a guarded pull-request change has no canonical baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchPullRequestMissingPolicyV1 {
    HydrateBeforeApplying,
    RefreshInstead,
}

/// Field-labeled pull-request context decoded from one delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookPullRequestContextV1Input {
    pub number: PullRequestNumber,
    pub head_sha: CommitSha,
    pub head_repository: Option<RepositorySlug>,
    pub base_branch: BranchName,
    pub head_branch: BranchName,
    pub title: PullRequestTitle,
    pub body: PullRequestBody,
    pub labels: Vec<LabelName>,
    pub draft: bool,
    pub author: Option<RepoWatchAuthorLogin>,
}

/// One delivery's pull-request context, with the head repository GitHub omits
/// once a tracked fork is deleted resolved during application rather than
/// required at decode time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookPullRequestContextV1 {
    input: RepoWatchWebhookPullRequestContextV1Input,
}

impl RepoWatchWebhookPullRequestContextV1 {
    pub fn new(input: RepoWatchWebhookPullRequestContextV1Input) -> Self {
        Self { input }
    }

    pub const fn number(&self) -> PullRequestNumber {
        self.input.number
    }

    pub const fn head_sha(&self) -> &CommitSha {
        &self.input.head_sha
    }

    pub fn head_repository(&self) -> Option<&RepositorySlug> {
        self.input.head_repository.as_ref()
    }

    /// The canonical context when the delivery itself named a head repository.
    pub fn delivered(&self) -> Option<PullRequestEventContext> {
        self.input
            .head_repository
            .clone()
            .map(|head_repository| self.canonical(head_repository))
    }

    /// The canonical context, reusing `retained` exactly when the delivery
    /// omitted the head repository of a deleted fork.
    pub fn with_retained_head_repository(
        &self,
        retained: &RepositorySlug,
    ) -> PullRequestEventContext {
        self.canonical(
            self.input
                .head_repository
                .clone()
                .unwrap_or_else(|| retained.clone()),
        )
    }

    fn canonical(&self, head_repository: RepositorySlug) -> PullRequestEventContext {
        PullRequestEventContext::new(PullRequestEventContextInput {
            number: self.input.number,
            head_sha: self.input.head_sha.clone(),
            head_repository,
            base_branch: self.input.base_branch.clone(),
            head_branch: self.input.head_branch.clone(),
            title: self.input.title.clone(),
            body: self.input.body.clone(),
            labels: self.input.labels.clone(),
            draft: self.input.draft,
            author: self.input.author.clone(),
        })
    }
}

/// Guard applied before replacing pull-request context from one delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchPullRequestHeadGuardV1 {
    AbsentOrMatching(CommitSha),
    Expected(CommitSha),
}

/// One closed, guarded mutation of the latest canonical observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchObservationChangeV1 {
    PullRequestContext {
        context: RepoWatchWebhookPullRequestContextV1,
        lifecycle: Option<RepoWatchPullRequestLifecycle>,
        head_guard: RepoWatchPullRequestHeadGuardV1,
        missing: RepoWatchPullRequestMissingPolicyV1,
    },
    ReviewUnion {
        pull_request: PullRequestNumber,
        expected_head: CommitSha,
        review: RepoWatchReviewObservation,
    },
    ThreadState {
        pull_request: PullRequestNumber,
        expected_head: CommitSha,
        thread: RepoWatchThreadObservation,
    },
    CheckRunUnion {
        pull_request: PullRequestNumber,
        expected_head: CommitSha,
        check_run: RepoWatchCheckRunObservation,
    },
    WorkflowRun {
        run: RepoWatchWorkflowRunObservation,
    },
    BranchHead {
        previous: RepoWatchBranchHeadPreviousV1,
        current: RepoWatchBranchHead,
    },
    BranchDeleted {
        branch: BranchName,
        expected_previous: CommitSha,
    },
}

/// Guard for replacing a branch head, including a newly created branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchBranchHeadPreviousV1 {
    Absent,
    Expected(CommitSha),
}

/// One provider query required because a delivery lacks canonical state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchTargetedRefreshV1 {
    PullRequestHydration {
        pull_request: PullRequestNumber,
    },
    Mergeability {
        pull_request: PullRequestNumber,
        expected_head: CommitSha,
    },
    CheckRollup {
        pull_request: PullRequestNumber,
        expected_head: CommitSha,
    },
    CheckRollupForCommit {
        head: CommitSha,
    },
}

/// Whole-pull-request hydrations one drained delivery page has already issued.
///
/// Every delivery on a page is durably admitted before the page is read, so the
/// provider state each one reports is already in place when the page issues its
/// first refresh, and one hydration observes all of them. Repeating that
/// hydration per delivery re-reads the same state on the shared polling
/// credential: pull-request detail, check suites, check runs, reviews, threads,
/// and one request per comment for that comment's reactions. Pull-request
/// comment deliveries put the repetition under an untrusted commenter's
/// control, since signature verification, delivery deduplication, and GitHub's
/// per-hook rate limit all admit repeated comment creation, edit, and deletion
/// as distinct legitimate deliveries.
///
/// Only whole-pull-request hydration coalesces. A mergeability or check-rollup
/// refresh names the commit it expects and an earlier hydration carries no
/// evidence about that commit, so those reach the poller with their guards
/// intact. The scope is one page and never a whole drain: a later page may hold
/// deliveries admitted after this page's hydration ran, reporting state that
/// hydration cannot have observed.
///
/// Asking and recording are separate because only a hydration that reached the
/// provider makes a later one redundant. A refresh that fails before its fetch
/// or before its commit leaves its delivery pending for a later drain, so
/// recording the ask rather than the landing would leave the page suppressing a
/// hydration that never happened. Success alone is not enough either, which is why [`record_issued`]
/// takes the whole submission: the runtime merges every refresh naming one pull
/// request into a single request carrying the strictest head guard among them,
/// and a targeted poll whose guard no longer matches the provider head discards
/// what it fetched and still reports success.
///
/// [`record_issued`]: RepoWatchTargetedRefreshCoalescerV1::record_issued
#[derive(Debug)]
pub struct RepoWatchTargetedRefreshCoalescerV1 {
    hydrated: BTreeSet<PullRequestNumber>,
}

impl RepoWatchTargetedRefreshCoalescerV1 {
    /// Opens the coalescing scope that one drained delivery page owns.
    pub fn for_delivery_page() -> Self {
        Self {
            hydrated: BTreeSet::new(),
        }
    }

    /// Retains the refreshes this page has not already issued.
    pub fn unissued(
        &self,
        refreshes: &[RepoWatchTargetedRefreshV1],
    ) -> Vec<RepoWatchTargetedRefreshV1> {
        refreshes
            .iter()
            .filter(|refresh| match refresh {
                RepoWatchTargetedRefreshV1::PullRequestHydration { pull_request } => {
                    !self.hydrated.contains(pull_request)
                }
                RepoWatchTargetedRefreshV1::Mergeability { .. }
                | RepoWatchTargetedRefreshV1::CheckRollup { .. }
                | RepoWatchTargetedRefreshV1::CheckRollupForCommit { .. } => true,
            })
            .cloned()
            .collect()
    }

    /// Records the hydrations one submission issued with nothing to discard
    /// them.
    ///
    /// A submission asking for anything head-guarded records nothing: the
    /// merged request carries that guard, so a head the provider has already
    /// moved past leaves the hydration fetched and thrown away rather than
    /// applied, and the poll reports success either way. Recording less than
    /// was issued only costs a repeated hydration; recording more would
    /// suppress one that never landed.
    pub fn record_issued(&mut self, refreshes: &[RepoWatchTargetedRefreshV1]) {
        if refreshes.iter().any(Self::carries_head_guard) {
            return;
        }
        self.hydrated
            .extend(refreshes.iter().filter_map(Self::hydrated_pull_request));
    }

    fn carries_head_guard(refresh: &RepoWatchTargetedRefreshV1) -> bool {
        match refresh {
            RepoWatchTargetedRefreshV1::Mergeability { .. }
            | RepoWatchTargetedRefreshV1::CheckRollup { .. }
            | RepoWatchTargetedRefreshV1::CheckRollupForCommit { .. } => true,
            RepoWatchTargetedRefreshV1::PullRequestHydration { .. } => false,
        }
    }

    fn hydrated_pull_request(refresh: &RepoWatchTargetedRefreshV1) -> Option<PullRequestNumber> {
        match refresh {
            RepoWatchTargetedRefreshV1::PullRequestHydration { pull_request } => {
                Some(*pull_request)
            }
            RepoWatchTargetedRefreshV1::Mergeability { .. }
            | RepoWatchTargetedRefreshV1::CheckRollup { .. }
            | RepoWatchTargetedRefreshV1::CheckRollupForCommit { .. } => None,
        }
    }
}

/// Closed patch produced from one mapped delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchObservationPatchV1 {
    changes: Box<[RepoWatchObservationChangeV1]>,
    targeted_refreshes: Box<[RepoWatchTargetedRefreshV1]>,
}

impl RepoWatchObservationPatchV1 {
    fn new(
        changes: Vec<RepoWatchObservationChangeV1>,
        targeted_refreshes: Vec<RepoWatchTargetedRefreshV1>,
    ) -> Self {
        Self {
            changes: changes.into_boxed_slice(),
            targeted_refreshes: targeted_refreshes.into_boxed_slice(),
        }
    }

    pub fn changes(&self) -> &[RepoWatchObservationChangeV1] {
        &self.changes
    }

    pub fn targeted_refreshes(&self) -> &[RepoWatchTargetedRefreshV1] {
        &self.targeted_refreshes
    }
}

/// Result of applying one patch to the latest canonical observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchObservationApplyV1 {
    Applied(RepoWatchObservation),
    DuplicateState,
    Superseded,
    Ignored(RepoWatchWebhookIgnoredReasonV1),
    NeedsTargetedRefresh {
        observation: RepoWatchObservation,
        refreshes: Box<[RepoWatchTargetedRefreshV1]>,
    },
}

/// Internal-coherence failure while rebuilding canonical state after a patch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookApplyError {
    RepositoryState(RepoWatchRepositoryStateError),
    ConflictingImmutableFact(&'static str),
}

impl fmt::Display for RepoWatchWebhookApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepositoryState(error) => write!(
                formatter,
                "webhook patch produced noncanonical repository state: {error}"
            ),
            Self::ConflictingImmutableFact(fact) => {
                write!(formatter, "webhook patch conflicts with retained {fact}")
            }
        }
    }
}

impl Error for RepoWatchWebhookApplyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RepositoryState(error) => Some(error),
            Self::ConflictingImmutableFact(_) => None,
        }
    }
}

impl From<RepoWatchRepositoryStateError> for RepoWatchWebhookApplyError {
    fn from(value: RepoWatchRepositoryStateError) -> Self {
        Self::RepositoryState(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChangeApplyDispositionV1 {
    Applied,
    Duplicate,
    Superseded,
    Ignored(RepoWatchWebhookIgnoredReasonV1),
    NeedsRefresh(RepoWatchTargetedRefreshV1),
}

/// Applies a mapped delivery without letting the runtime restate observation guards.
pub fn apply_repo_watch_observation_patch_v1(
    previous: &RepoWatchObservation,
    patch: &RepoWatchObservationPatchV1,
) -> Result<RepoWatchObservationApplyV1, RepoWatchWebhookApplyError> {
    let mut state = RepoWatchRepositoryStateInput {
        pull_requests: previous.state().pull_requests().to_vec(),
        workflow_runs: previous.state().workflow_runs().to_vec(),
        branch_heads: previous.state().branch_heads().to_vec(),
    };
    let mut changed = false;
    let mut refreshes = patch.targeted_refreshes().to_vec();
    for change in patch.changes() {
        match apply_observation_change(&mut state, change)? {
            ChangeApplyDispositionV1::Applied => changed = true,
            ChangeApplyDispositionV1::Duplicate => {}
            ChangeApplyDispositionV1::Superseded => {
                return Ok(RepoWatchObservationApplyV1::Superseded);
            }
            ChangeApplyDispositionV1::Ignored(reason) => {
                return Ok(RepoWatchObservationApplyV1::Ignored(reason));
            }
            ChangeApplyDispositionV1::NeedsRefresh(refresh) => {
                if !refreshes.contains(&refresh) {
                    refreshes.push(refresh);
                }
            }
        }
    }
    if !changed && refreshes.is_empty() {
        return Ok(RepoWatchObservationApplyV1::DuplicateState);
    }
    let observation = RepoWatchObservation::new(
        previous.signal_reviewers().to_vec(),
        RepoWatchRepositoryState::try_new(state)?,
    );
    if refreshes.is_empty() {
        Ok(RepoWatchObservationApplyV1::Applied(observation))
    } else {
        Ok(RepoWatchObservationApplyV1::NeedsTargetedRefresh {
            observation,
            refreshes: refreshes.into_boxed_slice(),
        })
    }
}

fn apply_observation_change(
    state: &mut RepoWatchRepositoryStateInput,
    change: &RepoWatchObservationChangeV1,
) -> Result<ChangeApplyDispositionV1, RepoWatchWebhookApplyError> {
    match change {
        RepoWatchObservationChangeV1::PullRequestContext {
            context,
            lifecycle,
            head_guard,
            missing,
        } => apply_pull_request_context(state, context, *lifecycle, head_guard, *missing),
        RepoWatchObservationChangeV1::ReviewUnion {
            pull_request,
            expected_head,
            review,
        } => apply_review_union(state, *pull_request, expected_head, review),
        RepoWatchObservationChangeV1::ThreadState {
            pull_request,
            expected_head,
            thread,
        } => apply_thread_state(state, *pull_request, expected_head, thread),
        RepoWatchObservationChangeV1::CheckRunUnion {
            pull_request,
            expected_head,
            check_run,
        } => apply_check_run_union(state, *pull_request, expected_head, check_run),
        RepoWatchObservationChangeV1::WorkflowRun { run } => apply_workflow_run(state, run),
        RepoWatchObservationChangeV1::BranchHead { previous, current } => {
            Ok(apply_branch_head(state, previous, current))
        }
        RepoWatchObservationChangeV1::BranchDeleted {
            branch,
            expected_previous,
        } => Ok(apply_branch_deletion(state, branch, expected_previous)),
    }
}

fn apply_pull_request_context(
    state: &mut RepoWatchRepositoryStateInput,
    context: &RepoWatchWebhookPullRequestContextV1,
    lifecycle: Option<RepoWatchPullRequestLifecycle>,
    head_guard: &RepoWatchPullRequestHeadGuardV1,
    missing: RepoWatchPullRequestMissingPolicyV1,
) -> Result<ChangeApplyDispositionV1, RepoWatchWebhookApplyError> {
    let Some(index) = pull_request_index(state, context.number()) else {
        return apply_missing_pull_request_context(state, context, lifecycle, missing);
    };
    let previous = &state.pull_requests[index];
    // GitHub represents `pull_request.head.repo` as null once a tracked fork is
    // deleted, which the poll normalizer answers by retaining the canonical head
    // repository rather than dropping the observation.
    let delivered = context.with_retained_head_repository(previous.context().head_repository());
    let resulting_lifecycle = lifecycle.unwrap_or(previous.lifecycle());
    if previous.context() == &delivered && previous.lifecycle() == resulting_lifecycle {
        return Ok(ChangeApplyDispositionV1::Duplicate);
    }
    let guard_matches = match head_guard {
        RepoWatchPullRequestHeadGuardV1::AbsentOrMatching(expected)
        | RepoWatchPullRequestHeadGuardV1::Expected(expected) => {
            previous.context().head_sha() == expected
        }
    };
    if !guard_matches {
        return Ok(ChangeApplyDispositionV1::Superseded);
    }
    state.pull_requests[index] = rebuild_pull_request(
        previous,
        delivered,
        resulting_lifecycle,
        previous.completed_check_runs().to_vec(),
        previous.reviews().to_vec(),
        previous.threads().to_vec(),
    )?;
    Ok(ChangeApplyDispositionV1::Applied)
}

/// Applies a pull-request context change that has no canonical baseline yet.
///
/// A `HydrateBeforeApplying` delivery carries the pull request's complete opened
/// context, so the patch inserts it instead of leaving the observation unchanged
/// and projecting only a targeted query; the mapper's hydration refresh still
/// reconciles the canonical cursor. A delivery with neither a delivered head
/// repository nor a retained baseline has nothing to resolve one from and stays
/// refresh-only.
fn apply_missing_pull_request_context(
    state: &mut RepoWatchRepositoryStateInput,
    context: &RepoWatchWebhookPullRequestContextV1,
    lifecycle: Option<RepoWatchPullRequestLifecycle>,
    missing: RepoWatchPullRequestMissingPolicyV1,
) -> Result<ChangeApplyDispositionV1, RepoWatchWebhookApplyError> {
    let (
        RepoWatchPullRequestMissingPolicyV1::HydrateBeforeApplying,
        Some(lifecycle),
        Some(delivered),
    ) = (missing, lifecycle, context.delivered())
    else {
        return Ok(missing_pull_request_refresh(context.number()));
    };
    state.pull_requests.push(RepoWatchPullRequestState::try_new(
        RepoWatchPullRequestStateInput {
            context: delivered,
            lifecycle,
            mergeable_state: MergeableState::Unknown,
            completed_check_suites: Vec::new(),
            completed_check_runs: Vec::new(),
            reviews: Vec::new(),
            threads: Vec::new(),
            reactions: Vec::new(),
        },
    )?);
    Ok(ChangeApplyDispositionV1::Applied)
}

fn apply_review_union(
    state: &mut RepoWatchRepositoryStateInput,
    number: PullRequestNumber,
    expected_head: &CommitSha,
    review: &RepoWatchReviewObservation,
) -> Result<ChangeApplyDispositionV1, RepoWatchWebhookApplyError> {
    let Some(index) = pull_request_index(state, number) else {
        return Ok(missing_pull_request_refresh(number));
    };
    let previous = &state.pull_requests[index];
    if let Some(retained) = previous
        .reviews()
        .iter()
        .find(|item| item.id() == review.id())
    {
        if retained == review {
            return Ok(ChangeApplyDispositionV1::Duplicate);
        }
        if retained.state().is_none() {
            // A dismissed review is retained with no state; a submission
            // observed after the dismissal is history the dismissal already
            // superseded, not a conflicting identity.
            return Ok(ChangeApplyDispositionV1::Superseded);
        }
        return Err(RepoWatchWebhookApplyError::ConflictingImmutableFact(
            "review identity",
        ));
    }
    if previous.context().head_sha() != expected_head {
        return Ok(ChangeApplyDispositionV1::Superseded);
    }
    let mut reviews = previous.reviews().to_vec();
    reviews.push(review.clone());
    state.pull_requests[index] = rebuild_pull_request(
        previous,
        previous.context().clone(),
        previous.lifecycle(),
        previous.completed_check_runs().to_vec(),
        reviews,
        previous.threads().to_vec(),
    )?;
    Ok(ChangeApplyDispositionV1::Applied)
}

fn apply_thread_state(
    state: &mut RepoWatchRepositoryStateInput,
    number: PullRequestNumber,
    expected_head: &CommitSha,
    thread: &RepoWatchThreadObservation,
) -> Result<ChangeApplyDispositionV1, RepoWatchWebhookApplyError> {
    let Some(index) = pull_request_index(state, number) else {
        return Ok(missing_pull_request_refresh(number));
    };
    let previous = &state.pull_requests[index];
    let retained_index = previous
        .threads()
        .iter()
        .position(|item| item.thread() == thread.thread());
    if retained_index.is_some_and(|position| &previous.threads()[position] == thread) {
        return Ok(ChangeApplyDispositionV1::Duplicate);
    }
    if previous.context().head_sha() != expected_head {
        return Ok(ChangeApplyDispositionV1::Superseded);
    }
    let mut threads = previous.threads().to_vec();
    match retained_index {
        Some(position) => threads[position] = thread.clone(),
        None => threads.push(thread.clone()),
    }
    state.pull_requests[index] = rebuild_pull_request(
        previous,
        previous.context().clone(),
        previous.lifecycle(),
        previous.completed_check_runs().to_vec(),
        previous.reviews().to_vec(),
        threads,
    )?;
    Ok(ChangeApplyDispositionV1::Applied)
}

fn apply_check_run_union(
    state: &mut RepoWatchRepositoryStateInput,
    number: PullRequestNumber,
    expected_head: &CommitSha,
    check_run: &RepoWatchCheckRunObservation,
) -> Result<ChangeApplyDispositionV1, RepoWatchWebhookApplyError> {
    let Some(index) = pull_request_index(state, number) else {
        return Ok(missing_pull_request_refresh(number));
    };
    let previous = &state.pull_requests[index];
    // A completed run can be rerequested under the same provider identity with a
    // later completion generation or a different conclusion, and the differ
    // treats either as a new observable completion, so the retained run is
    // replaced under the same head guard instead of failing the patch.
    let retained_index = previous
        .completed_check_runs()
        .iter()
        .position(|item| item.id() == check_run.id());
    if let Some(position) = retained_index {
        let retained = &previous.completed_check_runs()[position];
        if retained == check_run {
            return Ok(ChangeApplyDispositionV1::Duplicate);
        }
        // A rerequest carries a later completion generation. One carrying an
        // earlier generation is a delayed original completion, and applying it
        // would regress the baseline and project a stale completion that the
        // following targeted poll can correct but never withdraw. An equal
        // generation still replaces, which is how a conclusion edit arrives.
        if check_run.completion_generation() < retained.completion_generation() {
            return Ok(ChangeApplyDispositionV1::Superseded);
        }
    }
    if previous.context().head_sha() != expected_head {
        return Ok(ChangeApplyDispositionV1::Superseded);
    }
    let mut check_runs = previous.completed_check_runs().to_vec();
    match retained_index {
        Some(position) => check_runs[position] = check_run.clone(),
        None => check_runs.push(check_run.clone()),
    }
    state.pull_requests[index] = rebuild_pull_request(
        previous,
        previous.context().clone(),
        previous.lifecycle(),
        check_runs,
        previous.reviews().to_vec(),
        previous.threads().to_vec(),
    )?;
    Ok(ChangeApplyDispositionV1::Applied)
}

// Polling admits a workflow run only for a branch in the repository's current
// branch set, so a run whose branch has since been deleted is a fact polling
// will never produce. Admitting it from a payload would leave a webhook-only
// observation that no reconciliation can ever match, and under a later write
// mode a dispatch target that no longer exists. The delivery is therefore
// ignored rather than queried: there is nothing to reconcile toward.
//
// The branch set read here is the one every earlier delivery has already been
// applied to, and deliveries are drained in receipt order, so a branch this
// stream itself created carries into every later run on it. What stays absent
// is a branch the stream never announced — created outside the mapped set, or
// before intake began — and for those the stream genuinely cannot project the
// run, which is what the poll-only parity row then reports.
fn apply_workflow_run(
    state: &mut RepoWatchRepositoryStateInput,
    run: &RepoWatchWorkflowRunObservation,
) -> Result<ChangeApplyDispositionV1, RepoWatchWebhookApplyError> {
    if !state
        .branch_heads
        .iter()
        .any(|branch_head| branch_head.branch() == run.branch())
    {
        return Ok(ChangeApplyDispositionV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::AbsentWorkflowBranch,
        ));
    }
    let run = canonical_workflow_run(state, run);
    let retained_index = state
        .workflow_runs
        .iter()
        .position(|item| item.branch() == run.branch() && item.workflow_id() == run.workflow_id());
    let Some(index) = retained_index else {
        state.workflow_runs.push(run);
        return Ok(ChangeApplyDispositionV1::Applied);
    };
    let retained = &state.workflow_runs[index];
    let retained_generation = (retained.id(), retained.attempt());
    let incoming_generation = (run.id(), run.attempt());
    if retained_generation == incoming_generation {
        if retained == &run {
            return Ok(ChangeApplyDispositionV1::Duplicate);
        }
        return Err(RepoWatchWebhookApplyError::ConflictingImmutableFact(
            "workflow-run generation",
        ));
    }
    if incoming_generation < retained_generation {
        return Ok(ChangeApplyDispositionV1::Superseded);
    }
    state.workflow_runs[index] = run;
    Ok(ChangeApplyDispositionV1::Applied)
}

/// Renames one delivered run to the workflow name canonical state already
/// carries for that workflow identity.
///
/// Polling names a run from the workflows endpoint's current name, while a
/// delivery carries the name the workflow held when it ran. The occurrence
/// content identity hashes that name, so a rename between the two would
/// otherwise split one occurrence into a webhook-only and a poll-only row.
fn canonical_workflow_run(
    state: &RepoWatchRepositoryStateInput,
    run: &RepoWatchWorkflowRunObservation,
) -> RepoWatchWorkflowRunObservation {
    let canonical = state
        .workflow_runs
        .iter()
        .find(|retained| retained.workflow_id() == run.workflow_id())
        .map(RepoWatchWorkflowRunObservation::workflow);
    match canonical {
        Some(workflow) if workflow != run.workflow() => RepoWatchWorkflowRunObservation::new(
            run.id(),
            run.workflow_id(),
            run.attempt(),
            run.branch().clone(),
            workflow.clone(),
            run.conclusion(),
        ),
        Some(_) | None => run.clone(),
    }
}

fn apply_branch_head(
    state: &mut RepoWatchRepositoryStateInput,
    previous: &RepoWatchBranchHeadPreviousV1,
    current: &RepoWatchBranchHead,
) -> ChangeApplyDispositionV1 {
    let retained_index = state
        .branch_heads
        .iter()
        .position(|item| item.branch() == current.branch());
    let Some(index) = retained_index else {
        return match previous {
            RepoWatchBranchHeadPreviousV1::Absent => {
                state.branch_heads.push(current.clone());
                ChangeApplyDispositionV1::Applied
            }
            RepoWatchBranchHeadPreviousV1::Expected(_) => ChangeApplyDispositionV1::Superseded,
        };
    };
    let retained = &state.branch_heads[index];
    if retained == current {
        return ChangeApplyDispositionV1::Duplicate;
    }
    let guard_matches = match previous {
        RepoWatchBranchHeadPreviousV1::Absent => false,
        RepoWatchBranchHeadPreviousV1::Expected(expected) => retained.head() == expected,
    };
    if !guard_matches {
        return ChangeApplyDispositionV1::Superseded;
    }
    state.branch_heads[index] = current.clone();
    ChangeApplyDispositionV1::Applied
}

fn apply_branch_deletion(
    state: &mut RepoWatchRepositoryStateInput,
    branch: &BranchName,
    expected_previous: &CommitSha,
) -> ChangeApplyDispositionV1 {
    let retained_index = state
        .branch_heads
        .iter()
        .position(|item| item.branch() == branch);
    let Some(index) = retained_index else {
        return ChangeApplyDispositionV1::Duplicate;
    };
    if state.branch_heads[index].head() != expected_previous {
        return ChangeApplyDispositionV1::Superseded;
    }
    state.branch_heads.remove(index);
    ChangeApplyDispositionV1::Applied
}

fn pull_request_index(
    state: &RepoWatchRepositoryStateInput,
    number: PullRequestNumber,
) -> Option<usize> {
    state
        .pull_requests
        .iter()
        .position(|pull_request| pull_request.context().number() == number)
}

fn missing_pull_request_refresh(number: PullRequestNumber) -> ChangeApplyDispositionV1 {
    ChangeApplyDispositionV1::NeedsRefresh(RepoWatchTargetedRefreshV1::PullRequestHydration {
        pull_request: number,
    })
}

fn rebuild_pull_request(
    previous: &RepoWatchPullRequestState,
    context: PullRequestEventContext,
    lifecycle: RepoWatchPullRequestLifecycle,
    completed_check_runs: Vec<RepoWatchCheckRunObservation>,
    reviews: Vec<RepoWatchReviewObservation>,
    threads: Vec<RepoWatchThreadObservation>,
) -> Result<RepoWatchPullRequestState, RepoWatchWebhookApplyError> {
    RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
        context,
        lifecycle,
        mergeable_state: previous.mergeable_state(),
        completed_check_suites: previous.completed_check_suites().to_vec(),
        completed_check_runs,
        reviews,
        threads,
        reactions: previous.reactions().to_vec(),
    })
    .map_err(Into::into)
}

/// A subscribed, mapped delivery that intentionally changes no observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookMappedNoChangeV1 {
    Ping,
    ReviewDismissed,
}

/// Why an authenticated delivery is safely outside the version-one mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookIgnoredReasonV1 {
    UnmappedEvent,
    UnmappedAction,
    NonBranchPush,
    ForeignWorkflowRepository,
    AbsentWorkflowBranch,
    AbsentWorkflowHeadRepository,
    AbsentWorkflowHeadBranch,
}

/// Mapping disposition for one signature-valid admitted delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookMappingV1 {
    Patch(RepoWatchObservationPatchV1),
    MappedNoChange(RepoWatchWebhookMappedNoChangeV1),
    Ignored(RepoWatchWebhookIgnoredReasonV1),
}

/// Why an admitted body could not be decoded into its declared event family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookMappingError {
    MalformedJson,
    MissingField(&'static str),
    InvalidField(&'static str),
    RepositoryMismatch,
    ActionMismatch,
}

impl fmt::Display for RepoWatchWebhookMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedJson => formatter.write_str("webhook body is not valid JSON"),
            Self::MissingField(field) => write!(formatter, "webhook body is missing {field}"),
            Self::InvalidField(field) => write!(formatter, "webhook body has invalid {field}"),
            Self::RepositoryMismatch => {
                formatter.write_str("webhook body repository does not match admitted repository")
            }
            Self::ActionMismatch => {
                formatter.write_str("webhook body action does not match admitted action")
            }
        }
    }
}

impl Error for RepoWatchWebhookMappingError {}

/// Maps an authenticated delivery without depending on HTTP or persistence types.
pub fn map_repo_watch_webhook_delivery_v1(
    delivery: &RepoWatchWebhookDeliveryV1,
    exact_body: &[u8],
) -> Result<RepoWatchWebhookMappingV1, RepoWatchWebhookMappingError> {
    let payload: Value = serde_json::from_slice(exact_body)
        .map_err(|_| RepoWatchWebhookMappingError::MalformedJson)?;
    let root = object(&payload, "payload")?;
    verify_repository(root, delivery.repository())?;
    verify_action(root, delivery.action())?;

    match delivery.event() {
        "pull_request" => map_pull_request(root, delivery.action()),
        "issue_comment" => map_issue_comment(root, delivery.action()),
        "pull_request_review" => map_review(root, delivery.action()),
        "pull_request_review_thread" => map_thread(root, delivery.action()),
        "check_run" => map_check_run(root, delivery.action(), delivery.repository()),
        "check_suite" => map_check_suite(root, delivery.action()),
        "workflow_run" => map_workflow_run(root, delivery.action(), delivery.repository()),
        "push" => map_push(root),
        "ping" => Ok(RepoWatchWebhookMappingV1::MappedNoChange(
            RepoWatchWebhookMappedNoChangeV1::Ping,
        )),
        _ => Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::UnmappedEvent,
        )),
    }
}

fn map_issue_comment(
    root: &Map<String, Value>,
    action: Option<&str>,
) -> Result<RepoWatchWebhookMappingV1, RepoWatchWebhookMappingError> {
    if !matches!(action, Some("created" | "edited" | "deleted")) {
        return Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::UnmappedAction,
        ));
    }
    let issue = object_at(root, &["issue"], "issue")?;
    let Some(pull_request) = issue.get("pull_request") else {
        return Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::UnmappedEvent,
        ));
    };
    object(pull_request, "issue.pull_request")?;
    let pull_request =
        positive_at(issue, &["number"], "issue.number").map(PullRequestNumber::new)?;
    Ok(RepoWatchWebhookMappingV1::Patch(
        RepoWatchObservationPatchV1::new(
            Vec::new(),
            vec![RepoWatchTargetedRefreshV1::PullRequestHydration { pull_request }],
        ),
    ))
}

fn verify_repository(
    root: &Map<String, Value>,
    admitted: &RepositorySlug,
) -> Result<(), RepoWatchWebhookMappingError> {
    let repository = string_at(root, &["repository", "full_name"], "repository.full_name")?;
    let repository = RepositorySlug::try_new(repository.to_owned())
        .map_err(|_| RepoWatchWebhookMappingError::InvalidField("repository.full_name"))?;
    if &repository != admitted {
        return Err(RepoWatchWebhookMappingError::RepositoryMismatch);
    }
    Ok(())
}

fn verify_action(
    root: &Map<String, Value>,
    admitted: Option<&str>,
) -> Result<(), RepoWatchWebhookMappingError> {
    let body_action = root.get("action").and_then(Value::as_str);
    if body_action != admitted {
        return Err(RepoWatchWebhookMappingError::ActionMismatch);
    }
    Ok(())
}

fn map_pull_request(
    root: &Map<String, Value>,
    action: Option<&str>,
) -> Result<RepoWatchWebhookMappingV1, RepoWatchWebhookMappingError> {
    let context = pull_request_context(root)?;
    let number = context.number();
    let head = context.head_sha().clone();
    let (lifecycle, head_guard, missing) = match action {
        Some("opened" | "reopened") => (
            Some(RepoWatchPullRequestLifecycle::Open),
            RepoWatchPullRequestHeadGuardV1::AbsentOrMatching(head.clone()),
            RepoWatchPullRequestMissingPolicyV1::HydrateBeforeApplying,
        ),
        Some("closed") => (
            Some(
                if bool_at(root, &["pull_request", "merged"], "pull_request.merged")? {
                    RepoWatchPullRequestLifecycle::Merged
                } else {
                    RepoWatchPullRequestLifecycle::Closed
                },
            ),
            RepoWatchPullRequestHeadGuardV1::Expected(head.clone()),
            RepoWatchPullRequestMissingPolicyV1::RefreshInstead,
        ),
        Some("synchronize") => (
            None,
            RepoWatchPullRequestHeadGuardV1::Expected(commit_at(root, &["before"], "before")?),
            RepoWatchPullRequestMissingPolicyV1::RefreshInstead,
        ),
        Some("labeled" | "unlabeled" | "edited" | "converted_to_draft" | "ready_for_review") => (
            None,
            RepoWatchPullRequestHeadGuardV1::Expected(head.clone()),
            RepoWatchPullRequestMissingPolicyV1::RefreshInstead,
        ),
        _ => {
            return Ok(RepoWatchWebhookMappingV1::Ignored(
                RepoWatchWebhookIgnoredReasonV1::UnmappedAction,
            ));
        }
    };
    let mut refreshes = Vec::new();
    match action {
        Some("opened" | "reopened") => {
            refreshes.push(RepoWatchTargetedRefreshV1::PullRequestHydration {
                pull_request: number,
            });
        }
        Some("synchronize") => {
            refreshes.push(RepoWatchTargetedRefreshV1::Mergeability {
                pull_request: number,
                expected_head: head.clone(),
            });
            refreshes.push(RepoWatchTargetedRefreshV1::CheckRollup {
                pull_request: number,
                expected_head: head,
            });
        }
        _ => {}
    }
    Ok(RepoWatchWebhookMappingV1::Patch(
        RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::PullRequestContext {
                context,
                lifecycle,
                head_guard,
                missing,
            }],
            refreshes,
        ),
    ))
}

fn map_review(
    root: &Map<String, Value>,
    action: Option<&str>,
) -> Result<RepoWatchWebhookMappingV1, RepoWatchWebhookMappingError> {
    if action == Some("dismissed") {
        return Ok(RepoWatchWebhookMappingV1::MappedNoChange(
            RepoWatchWebhookMappedNoChangeV1::ReviewDismissed,
        ));
    }
    if action != Some("submitted") {
        return Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::UnmappedAction,
        ));
    }
    let pull_request = pull_request_number(root)?;
    let expected_head = commit_at(
        root,
        &["pull_request", "head", "sha"],
        "pull_request.head.sha",
    )?;
    let review = object_at(root, &["review"], "review")?;
    let id = object_id_at(review, &["id"], "review.id")?;
    let reviewer = login_at(review, &["user", "login"], "review.user.login")?;
    let state = review_state(string_at(review, &["state"], "review.state")?)?;
    let commit = commit_at(review, &["commit_id"], "review.commit_id")?;
    Ok(RepoWatchWebhookMappingV1::Patch(
        RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::ReviewUnion {
                pull_request,
                expected_head,
                review: RepoWatchReviewObservation::new(id, reviewer, Some(state), commit),
            }],
            Vec::new(),
        ),
    ))
}

fn map_thread(
    root: &Map<String, Value>,
    action: Option<&str>,
) -> Result<RepoWatchWebhookMappingV1, RepoWatchWebhookMappingError> {
    let state = match action {
        Some("resolved") => RepoWatchThreadState::Resolved,
        Some("unresolved") => RepoWatchThreadState::Open,
        _ => {
            return Ok(RepoWatchWebhookMappingV1::Ignored(
                RepoWatchWebhookIgnoredReasonV1::UnmappedAction,
            ));
        }
    };
    let thread = object_at(root, &["thread"], "thread")?;
    let thread =
        ReviewThreadId::try_new(string_at(thread, &["node_id"], "thread.node_id")?.to_owned())
            .map_err(|_| RepoWatchWebhookMappingError::InvalidField("thread.node_id"))?;
    Ok(RepoWatchWebhookMappingV1::Patch(
        RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::ThreadState {
                pull_request: pull_request_number(root)?,
                expected_head: commit_at(
                    root,
                    &["pull_request", "head", "sha"],
                    "pull_request.head.sha",
                )?,
                thread: RepoWatchThreadObservation::new(thread, state),
            }],
            Vec::new(),
        ),
    ))
}

fn map_check_run(
    root: &Map<String, Value>,
    action: Option<&str>,
    repository: &RepositorySlug,
) -> Result<RepoWatchWebhookMappingV1, RepoWatchWebhookMappingError> {
    if action != Some("completed") {
        return Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::UnmappedAction,
        ));
    }
    let run = object_at(root, &["check_run"], "check_run")?;
    let head = commit_at(run, &["head_sha"], "check_run.head_sha")?;
    let matching_pull_requests = pull_request_numbers_for_repository(run, repository)?;
    let Some(&pull_request) = matching_pull_requests.as_slice().first() else {
        return Ok(check_rollup_for_commit(head));
    };
    if matching_pull_requests.len() != 1 {
        return Ok(check_rollup_for_commit(head));
    }
    let check_run = RepoWatchCheckRunObservation::new(
        object_id_at(run, &["id"], "check_run.id")?,
        completion_generation_at(run, &["completed_at"], "check_run.completed_at")?,
        CheckRunName::try_new(string_at(run, &["name"], "check_run.name")?.to_owned())
            .map_err(|_| RepoWatchWebhookMappingError::InvalidField("check_run.name"))?,
        check_conclusion(string_at(run, &["conclusion"], "check_run.conclusion")?)?,
    );
    Ok(RepoWatchWebhookMappingV1::Patch(
        RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::CheckRunUnion {
                pull_request,
                expected_head: head.clone(),
                check_run,
            }],
            vec![RepoWatchTargetedRefreshV1::CheckRollup {
                pull_request,
                expected_head: head,
            }],
        ),
    ))
}

fn check_rollup_for_commit(head: CommitSha) -> RepoWatchWebhookMappingV1 {
    RepoWatchWebhookMappingV1::Patch(RepoWatchObservationPatchV1::new(
        Vec::new(),
        vec![RepoWatchTargetedRefreshV1::CheckRollupForCommit { head }],
    ))
}

fn map_check_suite(
    root: &Map<String, Value>,
    action: Option<&str>,
) -> Result<RepoWatchWebhookMappingV1, RepoWatchWebhookMappingError> {
    if action != Some("completed") {
        return Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::UnmappedAction,
        ));
    }
    let head = commit_at(root, &["check_suite", "head_sha"], "check_suite.head_sha")?;
    Ok(check_rollup_for_commit(head))
}

fn map_workflow_run(
    root: &Map<String, Value>,
    action: Option<&str>,
    repository: &RepositorySlug,
) -> Result<RepoWatchWebhookMappingV1, RepoWatchWebhookMappingError> {
    if action != Some("completed") {
        return Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::UnmappedAction,
        ));
    }
    let run = object_at(root, &["workflow_run"], "workflow_run")?;
    let Some(head_repository) = optional_repository_at(
        run,
        &["head_repository", "full_name"],
        "workflow_run.head_repository.full_name",
    )?
    else {
        return Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::AbsentWorkflowHeadRepository,
        ));
    };
    if &head_repository != repository {
        return Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::ForeignWorkflowRepository,
        ));
    }
    let Some(head_branch) = optional_text_at(run, &["head_branch"], "workflow_run.head_branch")?
    else {
        return Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::AbsentWorkflowHeadBranch,
        ));
    };
    let head_branch = BranchName::try_new(head_branch.to_owned())
        .map_err(|_| RepoWatchWebhookMappingError::InvalidField("workflow_run.head_branch"))?;
    let run = RepoWatchWorkflowRunObservation::new(
        object_id_at(run, &["id"], "workflow_run.id")?,
        object_id_at(run, &["workflow_id"], "workflow_run.workflow_id")?,
        workflow_attempt_at(run, &["run_attempt"], "workflow_run.run_attempt")?,
        head_branch,
        WorkflowName::try_new(string_at(run, &["name"], "workflow_run.name")?.to_owned())
            .map_err(|_| RepoWatchWebhookMappingError::InvalidField("workflow_run.name"))?,
        check_conclusion(string_at(run, &["conclusion"], "workflow_run.conclusion")?)?,
    );
    Ok(RepoWatchWebhookMappingV1::Patch(
        RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::WorkflowRun { run }],
            Vec::new(),
        ),
    ))
}

fn map_push(
    root: &Map<String, Value>,
) -> Result<RepoWatchWebhookMappingV1, RepoWatchWebhookMappingError> {
    let reference = string_at(root, &["ref"], "ref")?;
    let Some(branch) = reference.strip_prefix("refs/heads/") else {
        return Ok(RepoWatchWebhookMappingV1::Ignored(
            RepoWatchWebhookIgnoredReasonV1::NonBranchPush,
        ));
    };
    let branch = BranchName::try_new(branch.to_owned())
        .map_err(|_| RepoWatchWebhookMappingError::InvalidField("ref"))?;
    let deleted = bool_at(root, &["deleted"], "deleted")?;
    let change = if deleted {
        RepoWatchObservationChangeV1::BranchDeleted {
            branch,
            expected_previous: commit_at(root, &["before"], "before")?,
        }
    } else {
        let previous = if bool_at(root, &["created"], "created")? {
            RepoWatchBranchHeadPreviousV1::Absent
        } else {
            RepoWatchBranchHeadPreviousV1::Expected(commit_at(root, &["before"], "before")?)
        };
        let after = commit_at(root, &["after"], "after")?;
        RepoWatchObservationChangeV1::BranchHead {
            previous,
            current: RepoWatchBranchHead::new(branch, after),
        }
    };
    Ok(RepoWatchWebhookMappingV1::Patch(
        RepoWatchObservationPatchV1::new(vec![change], Vec::new()),
    ))
}

fn pull_request_context(
    root: &Map<String, Value>,
) -> Result<RepoWatchWebhookPullRequestContextV1, RepoWatchWebhookMappingError> {
    let pull_request = object_at(root, &["pull_request"], "pull_request")?;
    let labels = array_at(pull_request, &["labels"], "pull_request.labels")?
        .iter()
        .map(|label| {
            let label = object(label, "pull_request.labels[]")?;
            LabelName::try_new(
                string_at(label, &["name"], "pull_request.labels[].name")?.to_owned(),
            )
            .map_err(|_| RepoWatchWebhookMappingError::InvalidField("pull_request.labels[].name"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let author = optional_text_at(pull_request, &["user", "login"], "pull_request.user.login")?
        .map(|login| {
            RepoWatchAuthorLogin::try_new(login.to_owned())
                .map_err(|_| RepoWatchWebhookMappingError::InvalidField("pull_request.user.login"))
        })
        .transpose()?;
    Ok(RepoWatchWebhookPullRequestContextV1::new(
        RepoWatchWebhookPullRequestContextV1Input {
            number: pull_request_number(root)?,
            head_sha: commit_at(pull_request, &["head", "sha"], "pull_request.head.sha")?,
            head_repository: optional_repository_at(
                pull_request,
                &["head", "repo", "full_name"],
                "pull_request.head.repo.full_name",
            )?,
            base_branch: branch_at(pull_request, &["base", "ref"], "pull_request.base.ref")?,
            head_branch: branch_at(pull_request, &["head", "ref"], "pull_request.head.ref")?,
            title: PullRequestTitle::try_new(
                string_at(pull_request, &["title"], "pull_request.title")?.to_owned(),
            )
            .map_err(|_| RepoWatchWebhookMappingError::InvalidField("pull_request.title"))?,
            body: PullRequestBody::try_new(
                nullable_string_at(pull_request, &["body"], "pull_request.body")?
                    .unwrap_or_default()
                    .to_owned(),
            )
            .map_err(|_| RepoWatchWebhookMappingError::InvalidField("pull_request.body"))?,
            labels,
            draft: bool_at(pull_request, &["draft"], "pull_request.draft")?,
            author,
        },
    ))
}

fn pull_request_number(
    root: &Map<String, Value>,
) -> Result<PullRequestNumber, RepoWatchWebhookMappingError> {
    positive_at(root, &["number"], "number").map(PullRequestNumber::new)
}

fn pull_request_numbers_for_repository(
    check_run: &Map<String, Value>,
    repository: &RepositorySlug,
) -> Result<Vec<PullRequestNumber>, RepoWatchWebhookMappingError> {
    array_at(check_run, &["pull_requests"], "check_run.pull_requests")?
        .iter()
        .filter_map(|pull_request| {
            let pull_request = match object(pull_request, "check_run.pull_requests[]") {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            let candidate = match github_repository_url_at(
                pull_request,
                &["base", "repo", "url"],
                "check_run.pull_requests[].base.repo.url",
            ) {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            if &candidate != repository {
                return None;
            }
            Some(
                positive_at(
                    pull_request,
                    &["number"],
                    "check_run.pull_requests[].number",
                )
                .map(PullRequestNumber::new),
            )
        })
        .collect()
}

fn github_repository_url_at(
    root: &Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<RepositorySlug, RepoWatchWebhookMappingError> {
    const GITHUB_REPOSITORY_API_PREFIX: &str = "https://api.github.com/repos/";

    let url = string_at(root, path, field)?;
    let slug = url
        .strip_prefix(GITHUB_REPOSITORY_API_PREFIX)
        .ok_or(RepoWatchWebhookMappingError::InvalidField(field))?;
    RepositorySlug::try_new(slug.to_owned())
        .map_err(|_| RepoWatchWebhookMappingError::InvalidField(field))
}

fn object<'value>(
    value: &'value Value,
    field: &'static str,
) -> Result<&'value Map<String, Value>, RepoWatchWebhookMappingError> {
    value
        .as_object()
        .ok_or(RepoWatchWebhookMappingError::InvalidField(field))
}

fn value_at<'value>(
    root: &'value Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<&'value Value, RepoWatchWebhookMappingError> {
    let mut value = root
        .get(path[0])
        .ok_or(RepoWatchWebhookMappingError::MissingField(field))?;
    for member in &path[1..] {
        value = value
            .as_object()
            .and_then(|object| object.get(*member))
            .ok_or(RepoWatchWebhookMappingError::MissingField(field))?;
    }
    Ok(value)
}

fn object_at<'value>(
    root: &'value Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<&'value Map<String, Value>, RepoWatchWebhookMappingError> {
    object(value_at(root, path, field)?, field)
}

fn array_at<'value>(
    root: &'value Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<&'value [Value], RepoWatchWebhookMappingError> {
    value_at(root, path, field)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or(RepoWatchWebhookMappingError::InvalidField(field))
}

fn string_at<'value>(
    root: &'value Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<&'value str, RepoWatchWebhookMappingError> {
    value_at(root, path, field)?
        .as_str()
        .ok_or(RepoWatchWebhookMappingError::InvalidField(field))
}

fn nullable_string_at<'value>(
    root: &'value Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<Option<&'value str>, RepoWatchWebhookMappingError> {
    let value = value_at(root, path, field)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(Some)
        .ok_or(RepoWatchWebhookMappingError::InvalidField(field))
}

fn bool_at(
    root: &Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<bool, RepoWatchWebhookMappingError> {
    value_at(root, path, field)?
        .as_bool()
        .ok_or(RepoWatchWebhookMappingError::InvalidField(field))
}

fn positive_at(
    root: &Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<NonZeroU64, RepoWatchWebhookMappingError> {
    value_at(root, path, field)?
        .as_u64()
        .and_then(NonZeroU64::new)
        .ok_or(RepoWatchWebhookMappingError::InvalidField(field))
}

fn object_id_at(
    root: &Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<GitHubObjectId, RepoWatchWebhookMappingError> {
    positive_at(root, path, field).map(GitHubObjectId::new)
}

fn workflow_attempt_at(
    root: &Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<RepoWatchWorkflowRunAttempt, RepoWatchWebhookMappingError> {
    positive_at(root, path, field).map(RepoWatchWorkflowRunAttempt::new)
}

fn commit_at(
    root: &Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<CommitSha, RepoWatchWebhookMappingError> {
    CommitSha::try_new(string_at(root, path, field)?.to_owned())
        .map_err(|_| RepoWatchWebhookMappingError::InvalidField(field))
}

fn branch_at(
    root: &Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<BranchName, RepoWatchWebhookMappingError> {
    BranchName::try_new(string_at(root, path, field)?.to_owned())
        .map_err(|_| RepoWatchWebhookMappingError::InvalidField(field))
}

/// Reads text GitHub may omit or null at any step of `path`.
///
/// The provider nulls whole intermediate objects, not just leaves:
/// `pull_request.head.repo` once a tracked fork is deleted, and
/// `pull_request.user` once an author's account is gone. The poll normalizer
/// accepts both shapes, so decoding treats an absent or null step as absent.
fn optional_text_at<'value>(
    root: &'value Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<Option<&'value str>, RepoWatchWebhookMappingError> {
    let mut value = root.get(path[0]);
    for member in &path[1..] {
        let Some(current) = value.filter(|current| !current.is_null()) else {
            return Ok(None);
        };
        value = current
            .as_object()
            .ok_or(RepoWatchWebhookMappingError::InvalidField(field))?
            .get(*member);
    }
    let Some(current) = value.filter(|current| !current.is_null()) else {
        return Ok(None);
    };
    current
        .as_str()
        .map(Some)
        .ok_or(RepoWatchWebhookMappingError::InvalidField(field))
}

fn optional_repository_at(
    root: &Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<Option<RepositorySlug>, RepoWatchWebhookMappingError> {
    let Some(slug) = optional_text_at(root, path, field)? else {
        return Ok(None);
    };
    RepositorySlug::try_new(slug.to_owned())
        .map(Some)
        .map_err(|_| RepoWatchWebhookMappingError::InvalidField(field))
}

fn login_at(
    root: &Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<RepoWatchAuthorLogin, RepoWatchWebhookMappingError> {
    RepoWatchAuthorLogin::try_new(string_at(root, path, field)?.to_owned())
        .map_err(|_| RepoWatchWebhookMappingError::InvalidField(field))
}

fn completion_generation_at(
    root: &Map<String, Value>,
    path: &[&str],
    field: &'static str,
) -> Result<RepoWatchCheckCompletionGeneration, RepoWatchWebhookMappingError> {
    RepoWatchCheckCompletionGeneration::try_new(string_at(root, path, field)?.to_owned())
        .map_err(|_| RepoWatchWebhookMappingError::InvalidField(field))
}

fn review_state(value: &str) -> Result<ReviewState, RepoWatchWebhookMappingError> {
    match value {
        "approved" => Ok(ReviewState::Approved),
        "changes_requested" => Ok(ReviewState::ChangesRequested),
        "commented" => Ok(ReviewState::Commented),
        _ => Err(RepoWatchWebhookMappingError::InvalidField("review.state")),
    }
}

fn check_conclusion(value: &str) -> Result<CheckConclusion, RepoWatchWebhookMappingError> {
    match value {
        "success" => Ok(CheckConclusion::Success),
        "failure" => Ok(CheckConclusion::Failure),
        "neutral" => Ok(CheckConclusion::Neutral),
        "cancelled" => Ok(CheckConclusion::Cancelled),
        "skipped" => Ok(CheckConclusion::Skipped),
        "timed_out" => Ok(CheckConclusion::TimedOut),
        "action_required" => Ok(CheckConclusion::ActionRequired),
        "stale" => Ok(CheckConclusion::Stale),
        "startup_failure" => Ok(CheckConclusion::StartupFailure),
        _ => Err(RepoWatchWebhookMappingError::InvalidField("conclusion")),
    }
}

#[cfg(test)]
mod tests {
    use std::slice;

    use crate::RepoWatchReactionObservation;
    use signalbox_domain::{
        MergeableState, ReactionContent, ReactionSubject, RepoWatchAuthorLogin,
    };

    use super::*;

    const REPOSITORY: &str = "octo/example";
    const OTHER_REPOSITORY: &str = "contributor/example";
    const INITIAL_HEAD: &str = "1111111111111111111111111111111111111111";
    const CURRENT_HEAD: &str = "2222222222222222222222222222222222222222";
    const DELIVERY_ID: Uuid = Uuid::from_u128(1);
    const HOOK_ID: NonZeroU64 = NonZeroU64::new(7).expect("hook fixture is positive");
    const RECEIPT_SEQUENCE: NonZeroU64 = NonZeroU64::new(11).expect("receipt fixture is positive");
    const PULL_REQUEST: u64 = 17;
    const OTHER_PULL_REQUEST: u64 = 23;
    const RETAINED_CHECK_RUN: u64 = 801;
    const RETAINED_REVIEW: u64 = 802;
    const RETAINED_WORKFLOW_RUN: u64 = 803;
    const RETAINED_WORKFLOW_ATTEMPT: u64 = 2;
    const OLDER_WORKFLOW_ATTEMPT: u64 = 1;
    const NEWER_WORKFLOW_ATTEMPT: u64 = 3;
    const DIFFERENT_WORKFLOW_RUN: u64 = 905;
    const NEW_CHECK_RUN: u64 = 903;
    const RETAINED_WORKFLOW: u64 = 804;
    const MAPPED_REVIEW: u64 = 401;
    const UNIONED_REVIEW: u64 = 901;
    const STALE_REVIEW: u64 = 902;
    const MAPPED_CHECK_RUN: u64 = 501;
    const MAPPED_WORKFLOW_RUN: u64 = 601;
    const MAPPED_WORKFLOW: u64 = 602;
    const MAPPED_WORKFLOW_ATTEMPT: u64 = 2;
    const MAPPED_ISSUE_COMMENT: u64 = 721;
    const ORDINARY_ISSUE: u64 = 19;
    const ORDINARY_ISSUE_COMMENT: u64 = 722;
    const RETAINED_THREAD: &str = "PRRT_retained";
    const MAPPED_THREAD: &str = "PRRT_fixture";
    const SIGNAL_REVIEWER: &str = "signal-reviewer";
    const MAPPED_REVIEWER: &str = "reviewer";
    const BASE_BRANCH: &str = "main";
    const DELETED_BRANCH: &str = "topic";
    const CREATED_BRANCH: &str = "new-branch";
    const MAPPED_CHECK_NAME: &str = "tests";
    const BODY_DIGEST_FILL: u8 = 3;

    fn delivery(event: &str, action: Option<&str>) -> RepoWatchWebhookDeliveryV1 {
        RepoWatchWebhookDeliveryV1::new(RepoWatchWebhookDeliveryV1Input {
            repository: RepositorySlug::try_new(String::from(REPOSITORY))
                .expect("repository fixture is valid"),
            hook_id: HOOK_ID,
            delivery_id: DELIVERY_ID,
            event: event.to_owned(),
            action: action.map(str::to_owned),
            receipt_sequence: RECEIPT_SEQUENCE,
            body_digest: [BODY_DIGEST_FILL; 32],
        })
    }

    fn pull_request_payload(action: &str, root_extra: &str) -> String {
        format!(
            r#"{{
                "action":"{action}",
                "number":{PULL_REQUEST},
                "repository":{{"full_name":"{REPOSITORY}"}}
                {root_extra},
                "pull_request":{{
                    "title":"Webhook ingestion",
                    "body":"Map exact provider facts.",
                    "draft":false,
                    "merged":false,
                    "user":{{"login":"Octo-Cat"}},
                    "labels":[{{"name":"ready"}}],
                    "base":{{"ref":"main"}},
                    "head":{{
                        "sha":"{CURRENT_HEAD}",
                        "ref":"feature/webhooks",
                        "repo":{{"full_name":"{OTHER_REPOSITORY}"}}
                    }}
                }}
            }}"#
        )
    }

    fn mapped_patch(
        event: &str,
        action: Option<&str>,
        payload: &str,
    ) -> RepoWatchObservationPatchV1 {
        let mapping =
            map_repo_watch_webhook_delivery_v1(&delivery(event, action), payload.as_bytes())
                .expect("fixture maps");
        let RepoWatchWebhookMappingV1::Patch(patch) = mapping else {
            panic!("fixture must produce a patch")
        };
        patch
    }

    fn object_id(value: u64) -> GitHubObjectId {
        GitHubObjectId::new(NonZeroU64::new(value).expect("fixture object identity is positive"))
    }

    fn pull_request_number() -> PullRequestNumber {
        PullRequestNumber::new(
            NonZeroU64::new(PULL_REQUEST).expect("fixture pull-request number is positive"),
        )
    }

    fn other_pull_request_number() -> PullRequestNumber {
        PullRequestNumber::new(
            NonZeroU64::new(OTHER_PULL_REQUEST).expect("fixture pull-request number is positive"),
        )
    }

    fn hydration(pull_request: PullRequestNumber) -> RepoWatchTargetedRefreshV1 {
        RepoWatchTargetedRefreshV1::PullRequestHydration { pull_request }
    }

    fn canonical_observation(head: &str) -> RepoWatchObservation {
        let payload = pull_request_payload("opened", "");
        let payload: Value = serde_json::from_str(&payload).expect("fixture JSON is valid");
        let root = object(&payload, "payload").expect("fixture root is an object");
        let context = pull_request_context(root)
            .expect("fixture context is valid")
            .delivered()
            .expect("fixture names its head repository");
        let reviewer = RepoWatchAuthorLogin::try_new(String::from(SIGNAL_REVIEWER))
            .expect("fixture reviewer is valid");
        let context = PullRequestEventContext::new(PullRequestEventContextInput {
            number: context.number(),
            head_sha: CommitSha::try_new(String::from(head)).expect("fixture SHA is valid"),
            head_repository: context.head_repository().clone(),
            base_branch: context.base_branch().clone(),
            head_branch: context.head_branch().clone(),
            title: context.title().clone(),
            body: context.body().clone(),
            labels: context.labels().to_vec(),
            draft: context.draft(),
            author: context.author().cloned(),
        });
        let pull_request = RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
            context,
            lifecycle: RepoWatchPullRequestLifecycle::Open,
            mergeable_state: MergeableState::Mergeable,
            completed_check_suites: Vec::new(),
            completed_check_runs: vec![RepoWatchCheckRunObservation::new(
                object_id(RETAINED_CHECK_RUN),
                RepoWatchCheckCompletionGeneration::try_new(String::from("2026-08-15T12:00:00Z"))
                    .expect("fixture completion generation is valid"),
                CheckRunName::try_new(String::from("retained-check"))
                    .expect("fixture check name is valid"),
                CheckConclusion::Success,
            )],
            reviews: vec![RepoWatchReviewObservation::new(
                object_id(RETAINED_REVIEW),
                reviewer.clone(),
                Some(ReviewState::Approved),
                CommitSha::try_new(String::from(head)).expect("fixture SHA is valid"),
            )],
            threads: vec![RepoWatchThreadObservation::new(
                ReviewThreadId::try_new(String::from(RETAINED_THREAD))
                    .expect("fixture thread is valid"),
                RepoWatchThreadState::Open,
            )],
            reactions: vec![RepoWatchReactionObservation::new(
                ReactionSubject::PullRequestBody,
                reviewer.clone(),
                ReactionContent::try_new(String::from("+1")).expect("fixture reaction is valid"),
            )],
        })
        .expect("fixture PR state is canonical");
        let workflow = RepoWatchWorkflowRunObservation::new(
            object_id(RETAINED_WORKFLOW_RUN),
            object_id(RETAINED_WORKFLOW),
            RepoWatchWorkflowRunAttempt::new(
                NonZeroU64::new(RETAINED_WORKFLOW_ATTEMPT)
                    .expect("fixture workflow attempt is positive"),
            ),
            BranchName::try_new(String::from("main")).expect("fixture branch is valid"),
            WorkflowName::try_new(String::from("retained-workflow"))
                .expect("fixture workflow name is valid"),
            CheckConclusion::Success,
        );
        RepoWatchObservation::new(
            vec![reviewer],
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
                pull_requests: vec![pull_request],
                workflow_runs: vec![workflow],
                branch_heads: vec![RepoWatchBranchHead::new(
                    BranchName::try_new(String::from("main")).expect("fixture branch is valid"),
                    CommitSha::try_new(String::from(head)).expect("fixture SHA is valid"),
                )],
            })
            .expect("fixture repository state is canonical"),
        )
    }

    #[test]
    fn opened_pr_requests_hydration_without_inventing_mergeability() {
        let payload = pull_request_payload("opened", "");
        let patch = mapped_patch("pull_request", Some("opened"), &payload);

        let [
            RepoWatchObservationChangeV1::PullRequestContext {
                context,
                lifecycle,
                head_guard: _,
                missing,
            },
        ] = patch.changes()
        else {
            panic!("opened PR must produce one context change")
        };
        assert_eq!(context.number().get(), PULL_REQUEST);
        assert_eq!(context.head_sha().as_str(), CURRENT_HEAD);
        assert_eq!(*lifecycle, Some(RepoWatchPullRequestLifecycle::Open));
        assert_eq!(
            *missing,
            RepoWatchPullRequestMissingPolicyV1::HydrateBeforeApplying
        );
        assert_eq!(
            patch.targeted_refreshes(),
            [RepoWatchTargetedRefreshV1::PullRequestHydration {
                pull_request: PullRequestNumber::new(
                    NonZeroU64::new(PULL_REQUEST).expect("fixture PR is positive")
                )
            }]
        );
    }

    #[test]
    fn synchronize_pr_schedules_mergeability_and_rollup_queries() {
        let payload =
            pull_request_payload("synchronize", &format!(r#","before":"{INITIAL_HEAD}""#));
        let patch = mapped_patch("pull_request", Some("synchronize"), &payload);

        assert_eq!(patch.changes().len(), 1);
        assert_eq!(patch.targeted_refreshes().len(), 2);
        let RepoWatchTargetedRefreshV1::Mergeability {
            expected_head: mergeability_head,
            ..
        } = &patch.targeted_refreshes()[0]
        else {
            panic!("first refresh must query mergeability")
        };
        let RepoWatchTargetedRefreshV1::CheckRollup {
            expected_head: rollup_head,
            ..
        } = &patch.targeted_refreshes()[1]
        else {
            panic!("second refresh must query the check rollup")
        };
        assert_eq!(mergeability_head.as_str(), CURRENT_HEAD);
        assert_eq!(rollup_head.as_str(), CURRENT_HEAD);
    }

    #[test]
    fn closed_merged_pr_carries_merged_lifecycle() {
        let payload =
            pull_request_payload("closed", "").replace("\"merged\":false", "\"merged\":true");
        let patch = mapped_patch("pull_request", Some("closed"), &payload);

        let [RepoWatchObservationChangeV1::PullRequestContext { lifecycle, .. }] = patch.changes()
        else {
            panic!("closed PR must produce one context change")
        };
        assert_eq!(*lifecycle, Some(RepoWatchPullRequestLifecycle::Merged));
    }

    #[test]
    fn created_pr_issue_comment_requests_poll_equivalent_projection() {
        let payload = format!(
            r#"{{
                "action":"created",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "issue":{{
                    "number":{PULL_REQUEST},
                    "pull_request":{{"url":"https://api.github.com/repos/{REPOSITORY}/pulls/{PULL_REQUEST}"}}
                }},
                "comment":{{"id":{MAPPED_ISSUE_COMMENT}}}
            }}"#
        );
        let patch = mapped_patch("issue_comment", Some("created"), &payload);

        assert!(patch.changes().is_empty());
        assert_eq!(
            patch.targeted_refreshes(),
            [RepoWatchTargetedRefreshV1::PullRequestHydration {
                pull_request: pull_request_number()
            }]
        );
    }

    #[test]
    fn ordinary_issue_comment_is_cheap_ignored_success() {
        let payload = format!(
            r#"{{
                "action":"created",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "issue":{{"number":{ORDINARY_ISSUE}}},
                "comment":{{"id":{ORDINARY_ISSUE_COMMENT}}}
            }}"#
        );
        let mapping = map_repo_watch_webhook_delivery_v1(
            &delivery("issue_comment", Some("created")),
            payload.as_bytes(),
        )
        .expect("ordinary issue comment is not an error");

        assert_eq!(
            mapping,
            RepoWatchWebhookMappingV1::Ignored(RepoWatchWebhookIgnoredReasonV1::UnmappedEvent)
        );
    }

    #[test]
    fn one_delivery_page_hydrates_a_pull_request_once() {
        let refresh = hydration(pull_request_number());
        let mut page = RepoWatchTargetedRefreshCoalescerV1::for_delivery_page();

        let first = page.unissued(slice::from_ref(&refresh));
        page.record_issued(&first);
        let repeat = page.unissued(slice::from_ref(&refresh));

        assert_eq!(first, vec![refresh]);
        assert!(repeat.is_empty());
    }

    #[test]
    fn one_delivery_page_hydrates_each_pull_request_it_names() {
        let other_refresh = hydration(other_pull_request_number());
        let mut page = RepoWatchTargetedRefreshCoalescerV1::for_delivery_page();
        page.record_issued(&[hydration(pull_request_number())]);

        let unissued = page.unissued(slice::from_ref(&other_refresh));

        assert_eq!(unissued, vec![other_refresh]);
    }

    #[test]
    fn coalesced_hydration_leaves_head_guarded_refreshes_alone() {
        let pull_request = pull_request_number();
        let expected_head =
            CommitSha::try_new(String::from(CURRENT_HEAD)).expect("fixture SHA is valid");
        let guarded_refreshes = [
            RepoWatchTargetedRefreshV1::Mergeability {
                pull_request,
                expected_head: expected_head.clone(),
            },
            RepoWatchTargetedRefreshV1::CheckRollup {
                pull_request,
                expected_head,
            },
        ];
        let mut page = RepoWatchTargetedRefreshCoalescerV1::for_delivery_page();
        page.record_issued(&[hydration(pull_request)]);

        let unissued = page.unissued(&guarded_refreshes);

        assert_eq!(unissued, guarded_refreshes.to_vec());
    }

    #[test]
    fn a_later_delivery_page_hydrates_again() {
        let refresh = hydration(pull_request_number());
        let mut drained = RepoWatchTargetedRefreshCoalescerV1::for_delivery_page();
        drained.record_issued(slice::from_ref(&refresh));
        let next = RepoWatchTargetedRefreshCoalescerV1::for_delivery_page();

        let unissued = next.unissued(slice::from_ref(&refresh));

        assert_eq!(unissued, vec![refresh]);
    }

    #[test]
    fn a_hydration_merged_under_a_head_guard_is_not_recorded() {
        let pull_request = pull_request_number();
        let expected_head =
            CommitSha::try_new(String::from(CURRENT_HEAD)).expect("fixture SHA is valid");
        let guarded_submission = [
            hydration(pull_request),
            RepoWatchTargetedRefreshV1::Mergeability {
                pull_request,
                expected_head,
            },
        ];
        let later = hydration(pull_request);
        let mut page = RepoWatchTargetedRefreshCoalescerV1::for_delivery_page();
        page.record_issued(&guarded_submission);

        let unissued = page.unissued(slice::from_ref(&later));

        assert_eq!(unissued, vec![later]);
    }

    #[test]
    fn a_hydration_that_never_reached_the_provider_stays_unissued() {
        let refresh = hydration(pull_request_number());
        let page = RepoWatchTargetedRefreshCoalescerV1::for_delivery_page();
        // The delivery asked, its refresh failed, and nothing was recorded.
        let asked = page.unissued(slice::from_ref(&refresh));

        let reissued = page.unissued(slice::from_ref(&refresh));

        assert_eq!(asked, vec![refresh.clone()]);
        assert_eq!(reissued, vec![refresh]);
    }

    #[test]
    fn submitted_review_unions_provider_review_identity() {
        let payload = format!(
            r#"{{
                "action":"submitted",
                "number":{PULL_REQUEST},
                "repository":{{"full_name":"{REPOSITORY}"}},
                "pull_request":{{"head":{{"sha":"{CURRENT_HEAD}"}}}},
                "review":{{
                    "id":{MAPPED_REVIEW},
                    "user":{{"login":"Reviewer"}},
                    "state":"approved",
                    "commit_id":"{CURRENT_HEAD}"
                }}
            }}"#
        );
        let patch = mapped_patch("pull_request_review", Some("submitted"), &payload);

        let [RepoWatchObservationChangeV1::ReviewUnion { review, .. }] = patch.changes() else {
            panic!("submitted review must produce one union")
        };
        assert_eq!(review.id().get(), MAPPED_REVIEW);
        assert_eq!(review.reviewer().as_str(), MAPPED_REVIEWER);
        assert_eq!(review.state(), Some(ReviewState::Approved));
    }

    #[test]
    fn dismissed_review_is_mapped_without_removing_retained_review() {
        let payload =
            format!(r#"{{"action":"dismissed","repository":{{"full_name":"{REPOSITORY}"}}}}"#);
        let mapping = map_repo_watch_webhook_delivery_v1(
            &delivery("pull_request_review", Some("dismissed")),
            payload.as_bytes(),
        )
        .expect("dismissal maps");

        assert_eq!(
            mapping,
            RepoWatchWebhookMappingV1::MappedNoChange(
                RepoWatchWebhookMappedNoChangeV1::ReviewDismissed
            )
        );
    }

    #[test]
    fn resolved_thread_sets_guarded_thread_state() {
        let payload = format!(
            r#"{{
                "action":"resolved",
                "number":{PULL_REQUEST},
                "repository":{{"full_name":"{REPOSITORY}"}},
                "pull_request":{{"head":{{"sha":"{CURRENT_HEAD}"}}}},
                "thread":{{"node_id":"{MAPPED_THREAD}"}}
            }}"#
        );
        let patch = mapped_patch("pull_request_review_thread", Some("resolved"), &payload);

        let [RepoWatchObservationChangeV1::ThreadState { thread, .. }] = patch.changes() else {
            panic!("resolved thread must produce one state change")
        };
        assert_eq!(thread.thread().as_str(), MAPPED_THREAD);
        assert_eq!(thread.state(), RepoWatchThreadState::Resolved);
    }

    #[test]
    fn completed_check_run_maps_directly_and_requests_rollup() {
        let payload = format!(
            r#"{{
                "action":"completed",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "check_run":{{
                    "id":{MAPPED_CHECK_RUN},
                    "head_sha":"{CURRENT_HEAD}",
                    "completed_at":"2026-08-15T12:30:00Z",
                    "name":"{MAPPED_CHECK_NAME}",
                    "conclusion":"success",
                    "pull_requests":[{{
                        "number":{PULL_REQUEST},
                        "base":{{"repo":{{"url":"https://api.github.com/repos/{REPOSITORY}"}}}}
                    }}]
                }}
            }}"#
        );
        let patch = mapped_patch("check_run", Some("completed"), &payload);

        let [RepoWatchObservationChangeV1::CheckRunUnion { check_run, .. }] = patch.changes()
        else {
            panic!("unambiguous check run must produce one union")
        };
        assert_eq!(check_run.id().get(), MAPPED_CHECK_RUN);
        assert_eq!(check_run.name().as_str(), MAPPED_CHECK_NAME);
        assert_eq!(check_run.conclusion(), CheckConclusion::Success);
        assert_eq!(patch.targeted_refreshes().len(), 1);
    }

    #[test]
    fn ambiguous_check_run_requests_commit_rollup_without_direct_change() {
        let payload = format!(
            r#"{{
                "action":"completed",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "check_run":{{
                    "head_sha":"{CURRENT_HEAD}",
                    "pull_requests":[]
                }}
            }}"#
        );
        let patch = mapped_patch("check_run", Some("completed"), &payload);

        assert!(patch.changes().is_empty());
        let [RepoWatchTargetedRefreshV1::CheckRollupForCommit { head }] =
            patch.targeted_refreshes()
        else {
            panic!("ambiguous check run must request one commit refresh")
        };
        assert_eq!(head.as_str(), CURRENT_HEAD);
    }

    #[test]
    fn completed_check_suite_is_targeted_only() {
        let payload = format!(
            r#"{{
                "action":"completed",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "check_suite":{{"head_sha":"{CURRENT_HEAD}"}}
            }}"#
        );
        let patch = mapped_patch("check_suite", Some("completed"), &payload);

        assert!(patch.changes().is_empty());
        assert_eq!(patch.targeted_refreshes().len(), 1);
    }

    #[test]
    fn completed_workflow_run_maps_for_watched_head_repository() {
        let payload = format!(
            r#"{{
                "action":"completed",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "workflow_run":{{
                    "id":{MAPPED_WORKFLOW_RUN},
                    "workflow_id":{MAPPED_WORKFLOW},
                    "run_attempt":{MAPPED_WORKFLOW_ATTEMPT},
                    "head_branch":"main",
                    "head_repository":{{"full_name":"{REPOSITORY}"}},
                    "name":"continuous-integration",
                    "conclusion":"failure"
                }}
            }}"#
        );
        let patch = mapped_patch("workflow_run", Some("completed"), &payload);

        let [RepoWatchObservationChangeV1::WorkflowRun { run }] = patch.changes() else {
            panic!("watched workflow must produce one change")
        };
        assert_eq!(run.id().get(), MAPPED_WORKFLOW_RUN);
        assert_eq!(run.workflow_id().get(), MAPPED_WORKFLOW);
        assert_eq!(run.attempt().get(), MAPPED_WORKFLOW_ATTEMPT);
        assert_eq!(run.conclusion(), CheckConclusion::Failure);
    }

    #[test]
    fn completed_workflow_run_with_absent_head_repository_is_ignored() {
        let payload = format!(
            r#"{{
                "action":"completed",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "workflow_run":{{
                    "id":{MAPPED_WORKFLOW_RUN},
                    "workflow_id":{MAPPED_WORKFLOW},
                    "run_attempt":{MAPPED_WORKFLOW_ATTEMPT},
                    "head_branch":"main",
                    "head_repository":null,
                    "name":"continuous-integration",
                    "conclusion":"failure"
                }}
            }}"#
        );
        let mapping = map_repo_watch_webhook_delivery_v1(
            &delivery("workflow_run", Some("completed")),
            payload.as_bytes(),
        )
        .expect("a deleted-fork workflow head is not an error");

        assert_eq!(
            mapping,
            RepoWatchWebhookMappingV1::Ignored(
                RepoWatchWebhookIgnoredReasonV1::AbsentWorkflowHeadRepository
            )
        );
    }

    #[test]
    fn completed_workflow_run_with_absent_head_branch_is_ignored() {
        let payload = format!(
            r#"{{
                "action":"completed",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "workflow_run":{{
                    "id":{MAPPED_WORKFLOW_RUN},
                    "workflow_id":{MAPPED_WORKFLOW},
                    "run_attempt":{MAPPED_WORKFLOW_ATTEMPT},
                    "head_branch":null,
                    "head_repository":{{"full_name":"{REPOSITORY}"}},
                    "name":"continuous-integration",
                    "conclusion":"failure"
                }}
            }}"#
        );
        let mapping = map_repo_watch_webhook_delivery_v1(
            &delivery("workflow_run", Some("completed")),
            payload.as_bytes(),
        )
        .expect("a deleted source ref is not an error");

        assert_eq!(
            mapping,
            RepoWatchWebhookMappingV1::Ignored(
                RepoWatchWebhookIgnoredReasonV1::AbsentWorkflowHeadBranch
            )
        );
    }

    #[test]
    fn nondeletion_branch_push_replaces_guarded_head() {
        let payload = format!(
            r#"{{
                "repository":{{"full_name":"{REPOSITORY}"}},
                "ref":"refs/heads/main",
                "before":"{INITIAL_HEAD}",
                "after":"{CURRENT_HEAD}",
                "created":false,
                "deleted":false
            }}"#
        );
        let patch = mapped_patch("push", None, &payload);

        let [RepoWatchObservationChangeV1::BranchHead { previous, current }] = patch.changes()
        else {
            panic!("branch push must produce one guarded replacement")
        };
        assert_eq!(
            previous,
            &RepoWatchBranchHeadPreviousV1::Expected(
                CommitSha::try_new(String::from(INITIAL_HEAD)).expect("fixture SHA is valid")
            )
        );
        assert_eq!(current.branch().as_str(), BASE_BRANCH);
        assert_eq!(current.head().as_str(), CURRENT_HEAD);
    }

    #[test]
    fn deletion_branch_push_never_decodes_zero_after_sha() {
        let payload = format!(
            r#"{{
                "repository":{{"full_name":"{REPOSITORY}"}},
                "ref":"refs/heads/topic",
                "before":"{INITIAL_HEAD}",
                "after":"0000000000000000000000000000000000000000",
                "created":false,
                "deleted":true
            }}"#
        );
        let patch = mapped_patch("push", None, &payload);

        let [
            RepoWatchObservationChangeV1::BranchDeleted {
                branch,
                expected_previous,
            },
        ] = patch.changes()
        else {
            panic!("deleted branch must produce one removal")
        };
        assert_eq!(branch.as_str(), DELETED_BRANCH);
        assert_eq!(expected_previous.as_str(), INITIAL_HEAD);
    }

    #[test]
    fn ping_is_mapped_endpoint_health_only() {
        let payload = format!(
            r#"{{"zen":"Keep it logically awesome.","repository":{{"full_name":"{REPOSITORY}"}}}}"#
        );
        let mapping =
            map_repo_watch_webhook_delivery_v1(&delivery("ping", None), payload.as_bytes())
                .expect("ping maps");

        assert_eq!(
            mapping,
            RepoWatchWebhookMappingV1::MappedNoChange(RepoWatchWebhookMappedNoChangeV1::Ping)
        );
    }

    #[test]
    fn broad_subscription_workflow_job_is_cheap_ignored_success() {
        let payload = format!(
            r#"{{
                "action":"completed",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "workflow_job":{{"id":701}}
            }}"#
        );
        let mapping = map_repo_watch_webhook_delivery_v1(
            &delivery("workflow_job", Some("completed")),
            payload.as_bytes(),
        )
        .expect("unknown authenticated event is not an error");

        assert_eq!(
            mapping,
            RepoWatchWebhookMappingV1::Ignored(RepoWatchWebhookIgnoredReasonV1::UnmappedEvent)
        );
    }

    #[test]
    fn unmapped_pull_request_action_is_ignored_without_guessing() {
        let payload = pull_request_payload("assigned", "");
        let mapping = map_repo_watch_webhook_delivery_v1(
            &delivery("pull_request", Some("assigned")),
            payload.as_bytes(),
        )
        .expect("unknown action is not an error");

        assert_eq!(
            mapping,
            RepoWatchWebhookMappingV1::Ignored(RepoWatchWebhookIgnoredReasonV1::UnmappedAction)
        );
    }

    #[test]
    fn admitted_repository_mismatch_fails_closed() {
        let payload = r#"{"repository":{"full_name":"other/repository"}}"#;
        let error = map_repo_watch_webhook_delivery_v1(&delivery("ping", None), payload.as_bytes())
            .expect_err("repository mismatch must fail");

        assert_eq!(error, RepoWatchWebhookMappingError::RepositoryMismatch);
    }

    #[test]
    fn admitted_action_mismatch_fails_closed() {
        let payload =
            format!(r#"{{"action":"queued","repository":{{"full_name":"{REPOSITORY}"}}}}"#);
        let error = map_repo_watch_webhook_delivery_v1(
            &delivery("workflow_job", Some("completed")),
            payload.as_bytes(),
        )
        .expect_err("action mismatch must fail");

        assert_eq!(error, RepoWatchWebhookMappingError::ActionMismatch);
    }

    #[test]
    fn synchronize_application_preserves_nested_state_and_requests_queries() {
        let previous = canonical_observation(INITIAL_HEAD);
        let payload =
            pull_request_payload("synchronize", &format!(r#","before":"{INITIAL_HEAD}""#));
        let patch = mapped_patch("pull_request", Some("synchronize"), &payload);
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("guarded synchronization applies");

        let RepoWatchObservationApplyV1::NeedsTargetedRefresh {
            observation,
            refreshes,
        } = outcome
        else {
            panic!("synchronization must await targeted queries")
        };
        let before = &previous.state().pull_requests()[0];
        let after = &observation.state().pull_requests()[0];
        assert_eq!(after.context().head_sha().as_str(), CURRENT_HEAD);
        assert_eq!(after.mergeable_state(), before.mergeable_state());
        assert_eq!(after.completed_check_runs(), before.completed_check_runs());
        assert_eq!(after.reviews(), before.reviews());
        assert_eq!(after.threads(), before.threads());
        assert_eq!(after.reactions(), before.reactions());
        assert_eq!(refreshes.as_ref(), patch.targeted_refreshes());
    }

    #[test]
    fn review_application_unions_once_then_reports_duplicate_state() {
        let previous = canonical_observation(CURRENT_HEAD);
        let payload = format!(
            r#"{{
                "action":"submitted",
                "number":{PULL_REQUEST},
                "repository":{{"full_name":"{REPOSITORY}"}},
                "pull_request":{{"head":{{"sha":"{CURRENT_HEAD}"}}}},
                "review":{{
                    "id":{UNIONED_REVIEW},
                    "user":{{"login":"Reviewer"}},
                    "state":"commented",
                    "commit_id":"{CURRENT_HEAD}"
                }}
            }}"#
        );
        let patch = mapped_patch("pull_request_review", Some("submitted"), &payload);
        let first =
            apply_repo_watch_observation_patch_v1(&previous, &patch).expect("new review applies");
        let RepoWatchObservationApplyV1::Applied(current) = first else {
            panic!("new review must apply directly")
        };
        let second = apply_repo_watch_observation_patch_v1(&current, &patch)
            .expect("equal review replay is valid");

        let [retained, unioned] = current.state().pull_requests()[0].reviews() else {
            panic!("one retained and one new review must remain")
        };
        assert_eq!(retained.id().get(), RETAINED_REVIEW);
        assert_eq!(unioned.id().get(), UNIONED_REVIEW);
        assert_eq!(second, RepoWatchObservationApplyV1::DuplicateState);
    }

    #[test]
    fn stale_review_application_is_superseded() {
        let previous = canonical_observation(INITIAL_HEAD);
        let review = RepoWatchReviewObservation::new(
            object_id(STALE_REVIEW),
            RepoWatchAuthorLogin::try_new(String::from("reviewer"))
                .expect("fixture reviewer is valid"),
            Some(ReviewState::Approved),
            CommitSha::try_new(String::from(CURRENT_HEAD)).expect("fixture SHA is valid"),
        );
        let patch = RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::ReviewUnion {
                pull_request: PullRequestNumber::new(
                    NonZeroU64::new(PULL_REQUEST).expect("fixture PR is positive"),
                ),
                expected_head: CommitSha::try_new(String::from(CURRENT_HEAD))
                    .expect("fixture SHA is valid"),
                review,
            }],
            Vec::new(),
        );
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("stale fact is a disposition, not an internal error");

        assert_eq!(outcome, RepoWatchObservationApplyV1::Superseded);
    }

    #[test]
    fn missing_opened_pull_request_applies_before_hydrating() {
        let previous = RepoWatchObservation::new(
            Vec::new(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput::default())
                .expect("empty repository state is canonical"),
        );
        let payload = pull_request_payload("opened", "");
        let patch = mapped_patch("pull_request", Some("opened"), &payload);
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("missing PR applies its delivered context");

        let RepoWatchObservationApplyV1::NeedsTargetedRefresh {
            observation,
            refreshes,
        } = outcome
        else {
            panic!("an opened PR must still request hydration")
        };
        let [applied] = observation.state().pull_requests() else {
            panic!("the delivered pull request must be applied")
        };
        assert_eq!(applied.context().number().get(), PULL_REQUEST);
        assert_eq!(applied.context().head_sha().as_str(), CURRENT_HEAD);
        assert_eq!(applied.lifecycle(), RepoWatchPullRequestLifecycle::Open);
        assert_eq!(refreshes.as_ref(), patch.targeted_refreshes());
    }

    #[test]
    fn a_later_delivery_applies_against_the_earlier_one_in_the_same_drain() {
        let empty = RepoWatchObservation::new(
            Vec::new(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput::default())
                .expect("empty repository state is canonical"),
        );
        let opened = mapped_patch(
            "pull_request",
            Some("opened"),
            &pull_request_payload("opened", ""),
        );
        let RepoWatchObservationApplyV1::NeedsTargetedRefresh { observation, .. } =
            apply_repo_watch_observation_patch_v1(&empty, &opened)
                .expect("the opened delivery applies its delivered context")
        else {
            panic!("an opened PR must still request hydration")
        };
        let labeled = mapped_patch(
            "pull_request",
            Some("labeled"),
            &pull_request_payload("labeled", "").replace(
                r#""labels":[{"name":"ready"}]"#,
                r#""labels":[{"name":"ready"},{"name":"urgent"}]"#,
            ),
        );

        let outcome = apply_repo_watch_observation_patch_v1(&observation, &labeled)
            .expect("the later delivery applies against the earlier observation");

        let RepoWatchObservationApplyV1::Applied(current) = outcome else {
            panic!("a label added after the opening must project")
        };
        let [applied] = current.state().pull_requests() else {
            panic!("the opened pull request must remain")
        };
        assert_eq!(applied.context().labels().len(), 2);
    }

    #[test]
    fn a_later_delivery_reaches_no_baseline_once_the_earlier_one_is_discarded() {
        let empty = RepoWatchObservation::new(
            Vec::new(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput::default())
                .expect("empty repository state is canonical"),
        );
        let labeled = mapped_patch(
            "pull_request",
            Some("labeled"),
            &pull_request_payload("labeled", ""),
        );

        let outcome = apply_repo_watch_observation_patch_v1(&empty, &labeled)
            .expect("a missing baseline is a disposition, not an internal error");

        // What discarding the earlier delivery's observation would cost: the
        // later delivery projects nothing and only asks for hydration.
        let RepoWatchObservationApplyV1::NeedsTargetedRefresh { observation, .. } = outcome else {
            panic!("a labeled delivery without a baseline must await hydration")
        };
        assert!(observation.state().pull_requests().is_empty());
    }

    #[test]
    fn missing_closed_pull_request_stays_refresh_only() {
        let previous = RepoWatchObservation::new(
            Vec::new(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput::default())
                .expect("empty repository state is canonical"),
        );
        let payload = pull_request_payload("closed", "");
        let patch = mapped_patch("pull_request", Some("closed"), &payload);
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("missing PR maps to hydration");

        let RepoWatchObservationApplyV1::NeedsTargetedRefresh {
            observation,
            refreshes,
        } = outcome
        else {
            panic!("a closed delivery without a baseline must await hydration")
        };
        assert!(observation.state().pull_requests().is_empty());
        assert_eq!(
            refreshes.as_ref(),
            [RepoWatchTargetedRefreshV1::PullRequestHydration {
                pull_request: PullRequestNumber::new(
                    NonZeroU64::new(PULL_REQUEST).expect("fixture PR is positive")
                )
            }]
        );
    }

    #[test]
    fn a_deleted_author_account_maps_without_an_author() {
        let previous = canonical_observation(CURRENT_HEAD);
        let payload = pull_request_payload("closed", "")
            .replace(r#""user":{"login":"Octo-Cat"}"#, r#""user":null"#);
        let patch = mapped_patch("pull_request", Some("closed"), &payload);
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("a deleted author is a provider shape, not a mapping failure");

        let RepoWatchObservationApplyV1::Applied(observation) = outcome else {
            panic!("a closed delivery for a retained PR must apply")
        };
        let [applied] = observation.state().pull_requests() else {
            panic!("the retained pull request must remain")
        };
        assert_eq!(applied.context().author(), None);
    }

    #[test]
    fn deleted_fork_pull_request_retains_the_canonical_head_repository() {
        let previous = canonical_observation(CURRENT_HEAD);
        let payload = pull_request_payload("closed", "").replace(
            &format!(r#""repo":{{"full_name":"{OTHER_REPOSITORY}"}}"#),
            r#""repo":null"#,
        );
        let patch = mapped_patch("pull_request", Some("closed"), &payload);
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("a deleted fork is a provider shape, not a mapping failure");

        let RepoWatchObservationApplyV1::Applied(observation) = outcome else {
            panic!("a closed delivery for a retained PR must apply")
        };
        let [applied] = observation.state().pull_requests() else {
            panic!("the retained pull request must remain")
        };
        assert_eq!(
            applied.context().head_repository().as_str(),
            OTHER_REPOSITORY
        );
        assert_eq!(applied.lifecycle(), RepoWatchPullRequestLifecycle::Closed);
    }

    #[test]
    fn rerequested_check_run_replaces_the_retained_completion() {
        let previous = canonical_observation(CURRENT_HEAD);
        let payload = format!(
            r#"{{
                "action":"completed",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "check_run":{{
                    "id":{RETAINED_CHECK_RUN},
                    "head_sha":"{CURRENT_HEAD}",
                    "completed_at":"2026-08-15T14:00:00Z",
                    "name":"retained-check",
                    "conclusion":"failure",
                    "pull_requests":[{{
                        "number":{PULL_REQUEST},
                        "base":{{"repo":{{"url":"https://api.github.com/repos/{REPOSITORY}"}}}}
                    }}]
                }}
            }}"#
        );
        let patch = mapped_patch("check_run", Some("completed"), &payload);
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("a rerequested run is a new completion, not an immutable conflict");

        let RepoWatchObservationApplyV1::NeedsTargetedRefresh { observation, .. } = outcome else {
            panic!("check run must retain its aggregate-rollup query")
        };
        let [replaced] = observation.state().pull_requests()[0].completed_check_runs() else {
            panic!("the retained run identity must not be duplicated")
        };
        assert_eq!(replaced.id().get(), RETAINED_CHECK_RUN);
        assert_eq!(replaced.conclusion(), CheckConclusion::Failure);
        assert_eq!(
            replaced.completion_generation().as_str(),
            "2026-08-15T14:00:00Z"
        );
    }

    #[test]
    fn created_branch_application_is_guarded_absent_and_idempotent() {
        let previous = canonical_observation(CURRENT_HEAD);
        let payload = format!(
            r#"{{
                "repository":{{"full_name":"{REPOSITORY}"}},
                "ref":"refs/heads/new-branch",
                "before":"0000000000000000000000000000000000000000",
                "after":"{CURRENT_HEAD}",
                "created":true,
                "deleted":false
            }}"#
        );
        let patch = mapped_patch("push", None, &payload);
        let first =
            apply_repo_watch_observation_patch_v1(&previous, &patch).expect("new branch applies");
        let RepoWatchObservationApplyV1::Applied(current) = first else {
            panic!("new branch must apply directly")
        };
        let second = apply_repo_watch_observation_patch_v1(&current, &patch)
            .expect("equal branch replay is valid");

        let [retained, created] = current.state().branch_heads() else {
            panic!("retained and created branch heads must remain")
        };
        assert_eq!(retained.branch().as_str(), BASE_BRANCH);
        assert_eq!(created.branch().as_str(), CREATED_BRANCH);
        assert_eq!(second, RepoWatchObservationApplyV1::DuplicateState);
    }

    #[test]
    fn branch_application_replaces_then_removes_the_guarded_head() {
        let previous = canonical_observation(INITIAL_HEAD);
        let advance_payload = format!(
            r#"{{
                "repository":{{"full_name":"{REPOSITORY}"}},
                "ref":"refs/heads/main",
                "before":"{INITIAL_HEAD}",
                "after":"{CURRENT_HEAD}",
                "created":false,
                "deleted":false
            }}"#
        );
        let advance = mapped_patch("push", None, &advance_payload);
        let advanced = apply_repo_watch_observation_patch_v1(&previous, &advance)
            .expect("guarded branch advance applies");
        let RepoWatchObservationApplyV1::Applied(current) = advanced else {
            panic!("guarded branch advance must apply directly")
        };
        let delete_payload = format!(
            r#"{{
                "repository":{{"full_name":"{REPOSITORY}"}},
                "ref":"refs/heads/main",
                "before":"{CURRENT_HEAD}",
                "after":"0000000000000000000000000000000000000000",
                "created":false,
                "deleted":true
            }}"#
        );
        let delete = mapped_patch("push", None, &delete_payload);
        let deleted = apply_repo_watch_observation_patch_v1(&current, &delete)
            .expect("guarded branch deletion applies");
        let RepoWatchObservationApplyV1::Applied(after_delete) = deleted else {
            panic!("guarded branch deletion must apply directly")
        };

        assert_eq!(
            current.state().branch_heads()[0].head().as_str(),
            CURRENT_HEAD
        );
        assert!(after_delete.state().branch_heads().is_empty());
    }

    #[test]
    fn check_run_application_unions_fact_and_retains_targeted_rollup() {
        let previous = canonical_observation(CURRENT_HEAD);
        let payload = format!(
            r#"{{
                "action":"completed",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "check_run":{{
                    "id":{NEW_CHECK_RUN},
                    "head_sha":"{CURRENT_HEAD}",
                    "completed_at":"2026-08-15T13:00:00Z",
                    "name":"new-check",
                    "conclusion":"failure",
                    "pull_requests":[{{
                        "number":{PULL_REQUEST},
                        "base":{{"repo":{{"url":"https://api.github.com/repos/{REPOSITORY}"}}}}
                    }}]
                }}
            }}"#
        );
        let patch = mapped_patch("check_run", Some("completed"), &payload);
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("new check-run fact applies");
        let RepoWatchObservationApplyV1::NeedsTargetedRefresh {
            observation,
            refreshes,
        } = outcome
        else {
            panic!("check run must retain its aggregate-rollup query")
        };

        let [retained, added] = observation.state().pull_requests()[0].completed_check_runs()
        else {
            panic!("retained and new check run must remain")
        };
        assert_eq!(
            retained,
            &previous.state().pull_requests()[0].completed_check_runs()[0]
        );
        assert_eq!(added.id().get(), NEW_CHECK_RUN);
        assert_eq!(refreshes.as_ref(), patch.targeted_refreshes());
    }

    #[test]
    fn newer_workflow_generation_replaces_retained_projection() {
        let previous = canonical_observation(CURRENT_HEAD);
        let newer = RepoWatchWorkflowRunObservation::new(
            object_id(RETAINED_WORKFLOW_RUN),
            object_id(RETAINED_WORKFLOW),
            RepoWatchWorkflowRunAttempt::new(
                NonZeroU64::new(NEWER_WORKFLOW_ATTEMPT)
                    .expect("fixture workflow attempt is positive"),
            ),
            BranchName::try_new(String::from("main")).expect("fixture branch is valid"),
            WorkflowName::try_new(String::from("retained-workflow"))
                .expect("fixture workflow name is valid"),
            CheckConclusion::Failure,
        );
        let expected = newer.clone();
        let patch = RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::WorkflowRun { run: newer }],
            Vec::new(),
        );
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("newer workflow applies");
        let RepoWatchObservationApplyV1::Applied(current) = outcome else {
            panic!("newer workflow must apply directly")
        };

        assert_eq!(current.state().workflow_runs(), [expected]);
    }

    #[test]
    fn delayed_older_check_run_generation_is_superseded() {
        let previous = canonical_observation(CURRENT_HEAD);
        let payload = format!(
            r#"{{
                "action":"completed",
                "repository":{{"full_name":"{REPOSITORY}"}},
                "check_run":{{
                    "id":{RETAINED_CHECK_RUN},
                    "head_sha":"{CURRENT_HEAD}",
                    "completed_at":"2026-08-15T11:00:00Z",
                    "name":"retained-check",
                    "conclusion":"failure",
                    "pull_requests":[{{
                        "number":{PULL_REQUEST},
                        "base":{{"repo":{{"url":"https://api.github.com/repos/{REPOSITORY}"}}}}
                    }}]
                }}
            }}"#
        );
        let patch = mapped_patch("check_run", Some("completed"), &payload);
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("a stale generation is a disposition, not an internal error");

        assert_eq!(outcome, RepoWatchObservationApplyV1::Superseded);
    }

    #[test]
    fn a_workflow_run_on_a_deleted_branch_is_ignored_without_projection() {
        let previous = canonical_observation(CURRENT_HEAD);
        let deleted_branch_run = RepoWatchWorkflowRunObservation::new(
            object_id(DIFFERENT_WORKFLOW_RUN),
            object_id(MAPPED_WORKFLOW),
            RepoWatchWorkflowRunAttempt::new(
                NonZeroU64::new(MAPPED_WORKFLOW_ATTEMPT)
                    .expect("fixture workflow attempt is positive"),
            ),
            BranchName::try_new(String::from(DELETED_BRANCH)).expect("fixture branch is valid"),
            WorkflowName::try_new(String::from("orphaned-workflow"))
                .expect("fixture workflow name is valid"),
            CheckConclusion::Success,
        );
        let patch = RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::WorkflowRun {
                run: deleted_branch_run,
            }],
            Vec::new(),
        );

        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("a deleted branch is a disposition, not an internal error");

        assert_eq!(
            outcome,
            RepoWatchObservationApplyV1::Ignored(
                RepoWatchWebhookIgnoredReasonV1::AbsentWorkflowBranch
            )
        );
    }

    #[test]
    fn renamed_workflow_completion_adopts_the_canonical_workflow_name() {
        let previous = canonical_observation(CURRENT_HEAD);
        let renamed = RepoWatchWorkflowRunObservation::new(
            object_id(RETAINED_WORKFLOW_RUN),
            object_id(RETAINED_WORKFLOW),
            RepoWatchWorkflowRunAttempt::new(
                NonZeroU64::new(NEWER_WORKFLOW_ATTEMPT)
                    .expect("fixture workflow attempt is positive"),
            ),
            BranchName::try_new(String::from("main")).expect("fixture branch is valid"),
            WorkflowName::try_new(String::from("renamed-workflow"))
                .expect("fixture workflow name is valid"),
            CheckConclusion::Failure,
        );
        let patch = RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::WorkflowRun { run: renamed }],
            Vec::new(),
        );
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("a renamed workflow applies");

        let RepoWatchObservationApplyV1::Applied(current) = outcome else {
            panic!("a newer generation must apply directly")
        };
        let [applied] = current.state().workflow_runs() else {
            panic!("the retained workflow identity must not be duplicated")
        };
        assert_eq!(
            applied.workflow(),
            previous.state().workflow_runs()[0].workflow()
        );
        assert_eq!(applied.attempt().get(), NEWER_WORKFLOW_ATTEMPT);
        assert_eq!(applied.conclusion(), CheckConclusion::Failure);
    }

    #[test]
    fn older_workflow_generation_is_superseded() {
        let previous = canonical_observation(CURRENT_HEAD);
        let older = RepoWatchWorkflowRunObservation::new(
            object_id(RETAINED_WORKFLOW_RUN),
            object_id(RETAINED_WORKFLOW),
            RepoWatchWorkflowRunAttempt::new(
                NonZeroU64::new(OLDER_WORKFLOW_ATTEMPT)
                    .expect("fixture workflow attempt is positive"),
            ),
            BranchName::try_new(String::from("main")).expect("fixture branch is valid"),
            WorkflowName::try_new(String::from("retained-workflow"))
                .expect("fixture workflow name is valid"),
            CheckConclusion::Failure,
        );
        let patch = RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::WorkflowRun { run: older }],
            Vec::new(),
        );
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("older workflow is a disposition, not an internal error");

        assert_eq!(outcome, RepoWatchObservationApplyV1::Superseded);
    }

    #[test]
    fn higher_workflow_run_identity_replaces_retained_projection() {
        let previous = canonical_observation(CURRENT_HEAD);
        let different_run = RepoWatchWorkflowRunObservation::new(
            object_id(DIFFERENT_WORKFLOW_RUN),
            object_id(RETAINED_WORKFLOW),
            RepoWatchWorkflowRunAttempt::new(
                NonZeroU64::new(OLDER_WORKFLOW_ATTEMPT)
                    .expect("fixture workflow attempt is positive"),
            ),
            BranchName::try_new(String::from("main")).expect("fixture branch is valid"),
            WorkflowName::try_new(String::from("retained-workflow"))
                .expect("fixture workflow name is valid"),
            CheckConclusion::Failure,
        );
        let patch = RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::WorkflowRun { run: different_run }],
            Vec::new(),
        );
        let outcome = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("higher workflow run identity applies");
        let RepoWatchObservationApplyV1::Applied(current) = outcome else {
            panic!("higher workflow run identity must apply directly")
        };

        assert_eq!(
            current.state().workflow_runs()[0].id().get(),
            DIFFERENT_WORKFLOW_RUN
        );
    }

    #[test]
    fn conflicting_immutable_review_identity_is_an_apply_error() {
        let previous = canonical_observation(CURRENT_HEAD);
        let conflicting = RepoWatchReviewObservation::new(
            object_id(RETAINED_REVIEW),
            RepoWatchAuthorLogin::try_new(String::from(SIGNAL_REVIEWER))
                .expect("fixture reviewer is valid"),
            Some(ReviewState::ChangesRequested),
            CommitSha::try_new(String::from(CURRENT_HEAD)).expect("fixture SHA is valid"),
        );
        let patch = RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::ReviewUnion {
                pull_request: PullRequestNumber::new(
                    NonZeroU64::new(PULL_REQUEST).expect("fixture PR is positive"),
                ),
                expected_head: CommitSha::try_new(String::from(CURRENT_HEAD))
                    .expect("fixture SHA is valid"),
                review: conflicting,
            }],
            Vec::new(),
        );
        let error = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect_err("provider identity cannot change meaning");

        assert_eq!(
            error,
            RepoWatchWebhookApplyError::ConflictingImmutableFact("review identity")
        );
    }

    #[test]
    fn a_submission_observed_after_dismissal_is_superseded() {
        let previous = canonical_observation(CURRENT_HEAD);
        let mut pull_requests = previous.state().pull_requests().to_vec();
        let reviewer = RepoWatchAuthorLogin::try_new(String::from(SIGNAL_REVIEWER))
            .expect("fixture reviewer is valid");
        let dismissed = RepoWatchReviewObservation::new(
            object_id(RETAINED_REVIEW),
            reviewer.clone(),
            None,
            CommitSha::try_new(String::from(CURRENT_HEAD)).expect("fixture SHA is valid"),
        );
        let retained = &pull_requests[0];
        let rebuilt = rebuild_pull_request(
            retained,
            retained.context().clone(),
            retained.lifecycle(),
            retained.completed_check_runs().to_vec(),
            vec![dismissed],
            retained.threads().to_vec(),
        )
        .expect("fixture rebuild is valid");
        pull_requests[0] = rebuilt;
        let previous = RepoWatchObservation::new(
            previous.signal_reviewers().to_vec(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
                pull_requests,
                workflow_runs: previous.state().workflow_runs().to_vec(),
                branch_heads: previous.state().branch_heads().to_vec(),
            })
            .expect("fixture state is valid"),
        );
        let late_submission = RepoWatchReviewObservation::new(
            object_id(RETAINED_REVIEW),
            reviewer,
            Some(ReviewState::Approved),
            CommitSha::try_new(String::from(CURRENT_HEAD)).expect("fixture SHA is valid"),
        );
        let patch = RepoWatchObservationPatchV1::new(
            vec![RepoWatchObservationChangeV1::ReviewUnion {
                pull_request: PullRequestNumber::new(
                    NonZeroU64::new(PULL_REQUEST).expect("fixture PR is positive"),
                ),
                expected_head: CommitSha::try_new(String::from(CURRENT_HEAD))
                    .expect("fixture SHA is valid"),
                review: late_submission,
            }],
            Vec::new(),
        );

        let applied = apply_repo_watch_observation_patch_v1(&previous, &patch)
            .expect("a dismissal supersedes its late submission");

        assert_eq!(applied, RepoWatchObservationApplyV1::Superseded);
    }
}
