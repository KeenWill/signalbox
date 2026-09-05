//! Pure repository-state comparison for the repository-watch event boundary.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    future::Future,
    num::NonZeroU64,
};

use crate::SubmitInputIdGenerator;

use sha2::{Digest, Sha256};

use signalbox_domain::{
    AcceptedInputId, BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha,
    ContextFrontierId, CreateSession, DeliveryRequest, DurableCommandId, GitHubObjectId,
    GoalTextError, GoalUserAction, GoalUserCommand, LabelName, MergeableState,
    ModelSelectionOverride, ModuleDispatch, PerInputConfigurationChoices, PreparedCreateSession,
    PullRequestEventContext, PullRequestNumber, ReactionChange, ReactionContent, ReactionSubject,
    RepoWatchActionV1, RepoWatchAuthorLogin, RepoWatchDispatchContextError, RepoWatchDispatchId,
    RepoWatchEvent, RepoWatchEventConstructionError, RepoWatchEventId, RepoWatchEventKindNameV1,
    RepoWatchEventKindV1, RepoWatchEventTarget, RepoWatchRule, RepoWatchRuleId,
    RepoWatchRuleVersion, RepoWatchSingletonScope, RepoWatchWorkflowRunAttempt, RepositorySlug,
    ReviewState, ReviewThreadId, SemanticTranscriptEntryId, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationProvenance, SessionId, SessionTemplateName,
    SessionTemplateProvenance, SubmitInput, TurnAttemptId, TurnId, UserContent, WorkflowName,
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

const REPO_WATCH_EVENT_CONTENT_IDENTITY_DOMAIN_V1: &[u8] =
    b"signalbox/repo-watch/event-content-identity/v1";
const REPO_WATCH_EVENT_STREAM_IDENTITY_DOMAIN_V1: &[u8] =
    b"signalbox/repo-watch/event-stream-identity/v1";
const REPO_WATCH_EVENT_IDENTIFIED_CONTENT_DOMAIN_V1: &[u8] =
    b"signalbox/repo-watch/event-identified-content/v1";
/// Largest number of recurring occurrence streams one repository's frontier may
/// carry.
///
/// A stream is created by the first occurrence of a recurring event on a
/// distinct subject: a pull request's own transitions, each of its labels, each
/// review thread, each base branch it advances onto, and each distinct
/// reaction. Every entry costs a 32-byte stream identity, an 8-byte sequence,
/// and an 8-byte owning pull-request number, so this ceiling bounds one
/// repository's frontier at roughly 48 MB of resident entry fields before map
/// overhead, and a durable frontier of the same order. That is the
/// point at which a single watched repository's identity state, rather than its
/// event history, becomes the dominant cost of watching it, and it is far above
/// what any real repository reaches: GitHub's largest public repositories have
/// produced fewer than a million pull requests in their lifetimes, and each
/// contributes streams only for the subjects it actually touches.
///
/// Refusing here is the safe direction. The alternative to refusing is reusing
/// an occurrence number, which mints a content identity that collides with an
/// already-durable one, so the differ stops rather than emit an identity that
/// does not identify its occurrence.
// numeric-bound: guard - prevents hostile identity fan-out from exhausting resident memory
const MAX_REPO_WATCH_EVENT_IDENTITY_STREAMS: usize = 1_000_000;

/// A source-independent SHA-256 identity for one normalized event occurrence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchEventContentIdentityV1([u8; 32]);

impl RepoWatchEventContentIdentityV1 {
    /// Rebuilds an identity from its stored 32 bytes.
    ///
    /// This is the storage decoder's constructor: it asserts nothing about the
    /// bytes beyond their length, because a stored identity was computed when
    /// its occurrence was derived and cannot be recomputed from the row.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The identity's 32 bytes, for storage or comparison.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Last assigned occurrence number for one recurring source-independent stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchEventIdentityFrontierEntryV1 {
    stream_identity: [u8; 32],
    sequence: NonZeroU64,
    pull_request_number: Option<PullRequestNumber>,
}

impl RepoWatchEventIdentityFrontierEntryV1 {
    /// Pairs one stream identity with the last sequence assigned on it.
    ///
    /// Entries are the frontier's durable form. Building one states no
    /// invariant on its own; the frontier checks uniqueness across entries when
    /// it is assembled.
    pub const fn new(stream_identity: [u8; 32], sequence: NonZeroU64) -> Self {
        Self {
            stream_identity,
            sequence,
            pull_request_number: None,
        }
    }

    /// Pairs one stream with its last sequence and the pull request owning it.
    ///
    /// A repository-global stream, such as a branch workflow run, belongs to no
    /// pull request and carries none.
    pub const fn for_pull_request(
        stream_identity: [u8; 32],
        sequence: NonZeroU64,
        pull_request_number: PullRequestNumber,
    ) -> Self {
        Self {
            stream_identity,
            sequence,
            pull_request_number: Some(pull_request_number),
        }
    }

    /// The domain-separated identity of the stream this entry counts.
    pub const fn stream_identity(&self) -> &[u8; 32] {
        &self.stream_identity
    }

    /// The last occurrence number assigned on this stream.
    ///
    /// Positive by construction: the first occurrence takes one, so a stored
    /// zero is a corrupt entry rather than an empty stream.
    pub const fn sequence(&self) -> NonZeroU64 {
        self.sequence
    }

    /// The pull request owning this recurring stream, when one does.
    pub const fn pull_request_number(&self) -> Option<PullRequestNumber> {
        self.pull_request_number
    }
}

/// One stream's counter and the subject whose retirement releases it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RepoWatchEventIdentityFrontierSequenceV1 {
    sequence: NonZeroU64,
    pull_request_number: Option<PullRequestNumber>,
}

/// Canonical per-repository occurrence counters carried by the durable cursor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepoWatchEventIdentityFrontierV1 {
    sequences: BTreeMap<[u8; 32], RepoWatchEventIdentityFrontierSequenceV1>,
}

impl RepoWatchEventIdentityFrontierV1 {
    /// Rebuilds a frontier from its stored entries.
    ///
    /// Rejects entries that repeat a stream, and entries beyond the stream
    /// ceiling, so a decoded frontier holds the same invariants as one the
    /// differ advanced.
    pub fn try_from_entries(
        entries: Vec<RepoWatchEventIdentityFrontierEntryV1>,
    ) -> Result<Self, RepoWatchEventIdentityFrontierError> {
        if entries.len() > MAX_REPO_WATCH_EVENT_IDENTITY_STREAMS {
            return Err(RepoWatchEventIdentityFrontierError::StreamLimit);
        }
        let mut sequences = BTreeMap::new();
        for entry in entries {
            if sequences
                .insert(
                    entry.stream_identity,
                    RepoWatchEventIdentityFrontierSequenceV1 {
                        sequence: entry.sequence,
                        pull_request_number: entry.pull_request_number,
                    },
                )
                .is_some()
            {
                return Err(RepoWatchEventIdentityFrontierError::DuplicateStream);
            }
        }
        Ok(Self { sequences })
    }

    /// The frontier's entries in canonical stream-identity order.
    ///
    /// Ordering is canonical so an encoded cursor payload is byte-stable, which
    /// the durable canonicalization check depends on.
    pub fn entries(
        &self,
    ) -> impl ExactSizeIterator<Item = RepoWatchEventIdentityFrontierEntryV1> + '_ {
        self.sequences
            .iter()
            .map(|(stream, entry)| match entry.pull_request_number {
                Some(number) => RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
                    *stream,
                    entry.sequence,
                    number,
                ),
                None => RepoWatchEventIdentityFrontierEntryV1::new(*stream, entry.sequence),
            })
    }

    fn advance(
        &mut self,
        stream_identity: [u8; 32],
        pull_request_number: Option<PullRequestNumber>,
    ) -> Result<NonZeroU64, RepoWatchEventIdentityFrontierError> {
        let next = match self.sequences.get(&stream_identity) {
            Some(entry) => entry
                .sequence
                .get()
                .checked_add(1)
                .and_then(NonZeroU64::new)
                .ok_or(RepoWatchEventIdentityFrontierError::SequenceExhausted)?,
            None => {
                if self.sequences.len() >= MAX_REPO_WATCH_EVENT_IDENTITY_STREAMS {
                    return Err(RepoWatchEventIdentityFrontierError::StreamLimit);
                }
                NonZeroU64::MIN
            }
        };
        self.sequences.insert(
            stream_identity,
            RepoWatchEventIdentityFrontierSequenceV1 {
                sequence: next,
                pull_request_number,
            },
        );
        Ok(next)
    }
}

/// Why an occurrence frontier could not represent another event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchEventIdentityFrontierError {
    /// Stored entries named one stream twice, so its sequence is ambiguous.
    DuplicateStream,
    /// The repository holds the most recurring streams a frontier may carry.
    ///
    /// Reached only when assembling a stored frontier or introducing a new
    /// stream; a stream already counted keeps advancing at the ceiling.
    StreamLimit,
    /// One stream assigned every occurrence number a `u64` can hold.
    SequenceExhausted,
}

impl fmt::Display for RepoWatchEventIdentityFrontierError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DuplicateStream => "repository-watch identity frontier repeats a stream",
            Self::StreamLimit => "repository-watch identity frontier exceeds 1000000 streams",
            Self::SequenceExhausted => "repository-watch identity occurrence sequence is exhausted",
        })
    }
}

impl Error for RepoWatchEventIdentityFrontierError {}

/// One normalized event paired with its source-independent content identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchEventOccurrenceV1 {
    event: RepoWatchEvent,
    content_identity: RepoWatchEventContentIdentityV1,
}

impl RepoWatchEventOccurrenceV1 {
    /// Pairs an event with an identity that is not derived from it.
    ///
    /// Production occurrences are built only by the differ, which computes the
    /// identity from the occurrence's own evidence; nothing downstream can
    /// recompute an identity to check it, so a wrong value here would become a
    /// durable row whose advertised identity does not identify its content.
    /// Fixtures that need a well-formed occurrence without running the differ
    /// use this under `test-support`, which no production build enables.
    #[cfg(feature = "test-support")]
    pub const fn from_parts(
        event: RepoWatchEvent,
        content_identity: RepoWatchEventContentIdentityV1,
    ) -> Self {
        Self {
            event,
            content_identity,
        }
    }

    /// The normalized event this occurrence states.
    pub const fn event(&self) -> &RepoWatchEvent {
        &self.event
    }

    /// The occurrence's source-independent content identity.
    ///
    /// Two producers deriving this occurrence produce this same value, which is
    /// what lets storage recognize a restatement of an already-recorded fact.
    pub const fn content_identity(&self) -> RepoWatchEventContentIdentityV1 {
        self.content_identity
    }

    /// Discards the identity and keeps the event.
    ///
    /// For callers that have already recorded or compared the identity; the
    /// pairing cannot be rebuilt afterwards outside the differ.
    pub fn into_event(self) -> RepoWatchEvent {
        self.event
    }
}

/// Provider lifecycle projection for one known pull request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchPullRequestLifecycle {
    Open,
    Closed,
    Merged,
}

// numeric-bound: guard - preserves the advertised provider check-generation wire grammar
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
    pub settled: bool,
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
    settled: bool,
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
            // Unknown is GitHub's pending state, not affirmative evidence that
            // the exact head is mergeable. An unsettled head likewise has not
            // finished registering and completing its exact-head checks.
            || input.mergeable_state != MergeableState::Mergeable
            || !input.settled
            || input.gating_check_count == 0
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
            settled: input.settled,
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

    pub const fn settled(&self) -> bool {
        self.settled
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
    /// Returns whether one opaque provider review-node identity is admissible.
    pub fn review_node_id_is_valid(value: &str) -> bool {
        !value.is_empty() && value.len() <= 256 && !value.contains('\0')
    }

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
            // An unsettled head has not finished registering and completing its
            // exact-head checks, so an empty non-green list is the absence of
            // evidence rather than evidence of a green head. Dismissing a
            // blocking review then races the checks that have yet to report.
            || !assessment.settled()
            // A head carrying no gating check at all presents the same empty
            // non-green list as a fully green one, which is why the reference
            // convergence rule counts it as blocked. Clearance must read that
            // evidence the same way: without this gate, the "only remaining
            // blocker" the dismissal claims to clear would be the sole gate the
            // head ever had, and the review would be dismissed off zero checks.
            || assessment.gating_check_count() == 0
            // Affirmative mergeability, not merely the absence of a known
            // conflict. `Unknown` is GitHub still computing the merge, so it is
            // the absence of evidence rather than evidence of a mergeable head,
            // and the durable planner's predicate requires
            // `mergeable_state = 'mergeable'` outright. Refusing only
            // `Conflicting` here would let this public constructor mint a
            // candidate the durable planner refuses, leaving the in-memory rule
            // looser than the SQL it is meant to mirror.
            || assessment.mergeable_state() != MergeableState::Mergeable
            || &reviewed_head_sha == assessment.head_sha()
            || !Self::review_node_id_is_valid(&review_node_id)
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

/// One completed check-suite key retained after its pull request merges.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchMergedCheckSuiteBaselineV1 {
    id: GitHubObjectId,
    completion_generation: RepoWatchCheckCompletionGeneration,
}

impl RepoWatchMergedCheckSuiteBaselineV1 {
    pub const fn new(
        id: GitHubObjectId,
        completion_generation: RepoWatchCheckCompletionGeneration,
    ) -> Self {
        Self {
            id,
            completion_generation,
        }
    }

    pub const fn id(&self) -> GitHubObjectId {
        self.id
    }

    pub const fn completion_generation(&self) -> &RepoWatchCheckCompletionGeneration {
        &self.completion_generation
    }
}

/// One completed check-run comparison key retained after merge.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchMergedCheckRunBaselineV1 {
    id: GitHubObjectId,
    completion_generation: RepoWatchCheckCompletionGeneration,
    conclusion: CheckConclusion,
}

impl RepoWatchMergedCheckRunBaselineV1 {
    pub const fn new(
        id: GitHubObjectId,
        completion_generation: RepoWatchCheckCompletionGeneration,
        conclusion: CheckConclusion,
    ) -> Self {
        Self {
            id,
            completion_generation,
            conclusion,
        }
    }

    pub const fn id(&self) -> GitHubObjectId {
        self.id
    }

    pub const fn completion_generation(&self) -> &RepoWatchCheckCompletionGeneration {
        &self.completion_generation
    }

    pub const fn conclusion(&self) -> CheckConclusion {
        self.conclusion
    }
}

/// Field-labeled construction input for one compact merged-PR baseline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchMergedPullRequestBaselineInputV1 {
    pub number: PullRequestNumber,
    pub head_sha: CommitSha,
    pub signal_reviewers: Vec<RepoWatchAuthorLogin>,
    pub labels: Vec<LabelName>,
    pub mergeable_state: MergeableState,
    pub completed_check_suites: Vec<RepoWatchMergedCheckSuiteBaselineV1>,
    pub completed_check_runs: Vec<RepoWatchMergedCheckRunBaselineV1>,
    pub review_ids: Vec<GitHubObjectId>,
    pub threads: Vec<RepoWatchThreadObservation>,
    pub reactions: Vec<RepoWatchReactionObservation>,
}

/// Minimal consecutive-comparison state for a merged pull request.
///
/// Full provider details leave the ordinary observation after merge, while
/// this baseline retains only members the differ needs to recognize a later
/// post-merge occurrence without replaying terminal history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchMergedPullRequestBaselineV1 {
    number: PullRequestNumber,
    head_sha: CommitSha,
    signal_reviewers: Box<[RepoWatchAuthorLogin]>,
    labels: Box<[LabelName]>,
    mergeable_state: MergeableState,
    completed_check_suites: Box<[RepoWatchMergedCheckSuiteBaselineV1]>,
    completed_check_runs: Box<[RepoWatchMergedCheckRunBaselineV1]>,
    review_ids: Box<[GitHubObjectId]>,
    threads: Box<[RepoWatchThreadObservation]>,
    reactions: Box<[RepoWatchReactionObservation]>,
}

impl RepoWatchMergedPullRequestBaselineV1 {
    pub fn try_new(
        mut input: RepoWatchMergedPullRequestBaselineInputV1,
    ) -> Result<Self, RepoWatchRepositoryStateError> {
        input.signal_reviewers.sort();
        input.signal_reviewers.dedup();
        input.labels.sort();
        input.labels.dedup();
        input
            .completed_check_suites
            .sort_by_key(RepoWatchMergedCheckSuiteBaselineV1::id);
        reject_duplicate_object_ids(
            &input.completed_check_suites,
            RepoWatchMergedCheckSuiteBaselineV1::id,
            RepoWatchRepositoryStateError::DuplicateCheckSuite,
        )?;
        input
            .completed_check_runs
            .sort_by_key(RepoWatchMergedCheckRunBaselineV1::id);
        reject_duplicate_object_ids(
            &input.completed_check_runs,
            RepoWatchMergedCheckRunBaselineV1::id,
            RepoWatchRepositoryStateError::DuplicateCheckRun,
        )?;
        input.review_ids.sort();
        reject_duplicate_object_ids(
            &input.review_ids,
            |id| *id,
            RepoWatchRepositoryStateError::DuplicateReview,
        )?;
        input
            .threads
            .sort_by(|left, right| left.thread().cmp(right.thread()));
        reject_duplicate_threads(&input.threads)?;
        input.reactions.retain(|reaction| {
            input
                .signal_reviewers
                .binary_search(reaction.reactor())
                .is_ok()
        });
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
            number: input.number,
            head_sha: input.head_sha,
            signal_reviewers: input.signal_reviewers.into_boxed_slice(),
            labels: input.labels.into_boxed_slice(),
            mergeable_state: input.mergeable_state,
            completed_check_suites: input.completed_check_suites.into_boxed_slice(),
            completed_check_runs: input.completed_check_runs.into_boxed_slice(),
            review_ids: input.review_ids.into_boxed_slice(),
            threads: input.threads.into_boxed_slice(),
            reactions: input.reactions.into_boxed_slice(),
        })
    }

    pub fn from_merged_state(
        state: &RepoWatchPullRequestState,
        signal_reviewers: &[RepoWatchAuthorLogin],
    ) -> Result<Option<Self>, RepoWatchRepositoryStateError> {
        if state.lifecycle() != RepoWatchPullRequestLifecycle::Merged {
            return Ok(None);
        }
        Self::try_new(RepoWatchMergedPullRequestBaselineInputV1 {
            number: state.context().number(),
            head_sha: state.context().head_sha().clone(),
            signal_reviewers: signal_reviewers.to_vec(),
            labels: state.context().labels().to_vec(),
            mergeable_state: state.mergeable_state(),
            completed_check_suites: state
                .completed_check_suites()
                .iter()
                .map(|suite| {
                    RepoWatchMergedCheckSuiteBaselineV1::new(
                        suite.id(),
                        suite.completion_generation().clone(),
                    )
                })
                .collect(),
            completed_check_runs: state
                .completed_check_runs()
                .iter()
                .map(|run| {
                    RepoWatchMergedCheckRunBaselineV1::new(
                        run.id(),
                        run.completion_generation().clone(),
                        run.conclusion(),
                    )
                })
                .collect(),
            review_ids: state
                .reviews()
                .iter()
                .map(RepoWatchReviewObservation::id)
                .collect(),
            threads: state.threads().to_vec(),
            reactions: state.reactions().to_vec(),
        })
        .map(Some)
    }

    pub const fn number(&self) -> PullRequestNumber {
        self.number
    }

    pub const fn head_sha(&self) -> &CommitSha {
        &self.head_sha
    }

    pub fn signal_reviewers(&self) -> &[RepoWatchAuthorLogin] {
        &self.signal_reviewers
    }

    pub fn labels(&self) -> &[LabelName] {
        &self.labels
    }

    pub const fn mergeable_state(&self) -> MergeableState {
        self.mergeable_state
    }

    pub fn completed_check_suites(&self) -> &[RepoWatchMergedCheckSuiteBaselineV1] {
        &self.completed_check_suites
    }

    pub fn completed_check_runs(&self) -> &[RepoWatchMergedCheckRunBaselineV1] {
        &self.completed_check_runs
    }

    pub fn review_ids(&self) -> &[GitHubObjectId] {
        &self.review_ids
    }

    pub fn threads(&self) -> &[RepoWatchThreadObservation] {
        &self.threads
    }

    pub fn reactions(&self) -> &[RepoWatchReactionObservation] {
        &self.reactions
    }
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
    MergedPullRequestBaselineLimit,
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
            Self::MergedPullRequestBaselineLimit => {
                formatter.write_str("repository-watch cursor exceeds 1000000 merged baselines")
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

enum RepoWatchEventStreamKeyV1<'value> {
    PullRequestKind {
        number: PullRequestNumber,
        kind: RepoWatchEventKindNameV1,
    },
    Label {
        number: PullRequestNumber,
        kind: RepoWatchEventKindNameV1,
        label: &'value LabelName,
    },
    CheckSuite {
        number: PullRequestNumber,
        suite: GitHubObjectId,
        completion_generation: &'value RepoWatchCheckCompletionGeneration,
    },
    CheckRun {
        number: PullRequestNumber,
        run: GitHubObjectId,
        completion_generation: &'value RepoWatchCheckCompletionGeneration,
    },
    Review {
        number: PullRequestNumber,
        review: GitHubObjectId,
    },
    Thread {
        number: PullRequestNumber,
        kind: RepoWatchEventKindNameV1,
        thread: &'value ReviewThreadId,
    },
    Workflow {
        branch: &'value BranchName,
        workflow: GitHubObjectId,
        run: GitHubObjectId,
        attempt: RepoWatchWorkflowRunAttempt,
    },
    BaseAdvance {
        number: PullRequestNumber,
        branch: &'value BranchName,
    },
    Reaction {
        number: PullRequestNumber,
        subject: ReactionSubject,
        reactor: &'value RepoWatchAuthorLogin,
        content: &'value ReactionContent,
        change: ReactionChange,
    },
}

impl RepoWatchEventStreamKeyV1<'_> {
    /// The pull request owning this stream, or none for a repository-global one.
    ///
    /// A workflow run belongs to a branch rather than to any pull request, so
    /// its stream has no subject whose terminal state could retire it.
    const fn pull_request_number(&self) -> Option<PullRequestNumber> {
        match self {
            Self::PullRequestKind { number, .. }
            | Self::Label { number, .. }
            | Self::CheckSuite { number, .. }
            | Self::CheckRun { number, .. }
            | Self::Review { number, .. }
            | Self::Thread { number, .. }
            | Self::BaseAdvance { number, .. }
            | Self::Reaction { number, .. } => Some(*number),
            Self::Workflow { .. } => None,
        }
    }

    /// Whether this stream can state more than one fact.
    ///
    /// A stream is non-recurring only when the differ suppresses re-emission on
    /// members the stream key already names, so a second occurrence cannot
    /// arise. Check suites key on suite and completion generation, reviews on
    /// the provider review identity, and workflow runs on branch, run, and
    /// attempt; none of those admit a second fact under one key.
    ///
    /// Check runs are recurring despite being provider-keyed. The differ
    /// re-emits when a completed run's conclusion changes under an unchanged
    /// identity and completion generation, so a run edited back to an earlier
    /// conclusion would otherwise restate that conclusion's exact content
    /// identity, and the commit would coalesce the restored conclusion away
    /// instead of announcing it.
    const fn is_recurring(&self) -> bool {
        match self {
            Self::PullRequestKind { .. }
            | Self::Label { .. }
            | Self::Thread { .. }
            | Self::BaseAdvance { .. }
            | Self::Reaction { .. }
            | Self::CheckRun { .. } => true,
            Self::CheckSuite { .. } | Self::Review { .. } | Self::Workflow { .. } => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepoWatchDifferFailure {
    BaselineCollection(PullRequestNumber),
    EventConstruction(RepoWatchEventConstructionError),
    IdentityFrontier(RepoWatchEventIdentityFrontierError),
}

/// Which part of derivation failed.
///
/// Lets a caller classify a derivation failure without reading `Display` text.
/// The two carry different operational meaning: event construction indicates a
/// differ defect on one observation, while an identity-frontier failure is a
/// property of the frontier the comparison ran against.
///
/// An identity-frontier failure is not by itself permanent, and a caller must
/// not retire a repository on one. `StreamLimit` refuses only a comparison that
/// introduces a stream the frontier has never counted; streams already counted
/// keep advancing at the ceiling, so a later observation adding no new stream
/// succeeds. `SequenceExhausted` is terminal for the single stream that
/// exhausted it, not for the repository.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchDifferFailureKind {
    /// Compact merged-pull-request baselines were not a unique collection.
    BaselineCollection,
    /// The differ assembled an event the domain rejects.
    EventConstruction,
    /// The occurrence frontier could not assign the next sequence.
    IdentityFrontier,
}

/// Internal coherence failure while deriving a domain event or its identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchDifferError(RepoWatchDifferFailure);

impl RepoWatchDifferError {
    /// Which part of derivation failed.
    pub const fn kind(&self) -> RepoWatchDifferFailureKind {
        match self.0 {
            RepoWatchDifferFailure::BaselineCollection(_) => {
                RepoWatchDifferFailureKind::BaselineCollection
            }
            RepoWatchDifferFailure::EventConstruction(_) => {
                RepoWatchDifferFailureKind::EventConstruction
            }
            RepoWatchDifferFailure::IdentityFrontier(_) => {
                RepoWatchDifferFailureKind::IdentityFrontier
            }
        }
    }
}

impl fmt::Display for RepoWatchDifferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            RepoWatchDifferFailure::BaselineCollection(number) => write!(
                formatter,
                "repository-watch differ received duplicate merged baseline {}",
                number.get()
            ),
            RepoWatchDifferFailure::EventConstruction(error) => write!(
                formatter,
                "repository-watch differ produced an invalid event: {error}"
            ),
            RepoWatchDifferFailure::IdentityFrontier(error) => write!(
                formatter,
                "repository-watch differ could not advance event identity: {error}"
            ),
        }
    }
}

impl Error for RepoWatchDifferError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.0 {
            RepoWatchDifferFailure::BaselineCollection(_) => None,
            RepoWatchDifferFailure::EventConstruction(error) => Some(error),
            RepoWatchDifferFailure::IdentityFrontier(error) => Some(error),
        }
    }
}

impl From<RepoWatchEventConstructionError> for RepoWatchDifferError {
    fn from(value: RepoWatchEventConstructionError) -> Self {
        Self(RepoWatchDifferFailure::EventConstruction(value))
    }
}

impl From<RepoWatchEventIdentityFrontierError> for RepoWatchDifferError {
    fn from(value: RepoWatchEventIdentityFrontierError) -> Self {
        Self(RepoWatchDifferFailure::IdentityFrontier(value))
    }
}

/// Compares consecutive accepted normalized observations into closed domain facts.
pub fn derive_repo_watch_events(
    repository: &RepositorySlug,
    previous: Option<&RepoWatchObservation>,
    current: &RepoWatchObservation,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
) -> Result<Vec<RepoWatchEventOccurrenceV1>, RepoWatchDifferError> {
    derive_repo_watch_events_with_merged_baselines(
        repository,
        previous,
        &[],
        current,
        identity_frontier,
        ids,
    )
}

/// Compares observations while preserving recurring events for compacted merged PRs.
pub fn derive_repo_watch_events_with_merged_baselines(
    repository: &RepositorySlug,
    previous: Option<&RepoWatchObservation>,
    merged_baselines: &[RepoWatchMergedPullRequestBaselineV1],
    current: &RepoWatchObservation,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
) -> Result<Vec<RepoWatchEventOccurrenceV1>, RepoWatchDifferError> {
    let mut events = Vec::new();
    let mut merged_baselines_by_number = BTreeMap::new();
    for baseline in merged_baselines {
        if merged_baselines_by_number
            .insert(baseline.number(), baseline)
            .is_some()
        {
            return Err(RepoWatchDifferError(
                RepoWatchDifferFailure::BaselineCollection(baseline.number()),
            ));
        }
    }
    let reaction_filter_unchanged =
        previous.is_none_or(|prior| prior.signal_reviewers() == current.signal_reviewers());
    for current_pull_request in current.state().pull_requests() {
        let previous_pull_request = previous.and_then(|prior| {
            find_pull_request(
                prior.state().pull_requests(),
                current_pull_request.context().number(),
            )
        });
        let repository_comparison = RepositoryComparison {
            previous: previous.map(RepoWatchObservation::state),
            current: current.state(),
            current_signal_reviewers: current.signal_reviewers(),
            reaction_filter_unchanged,
        };
        if let Some(previous_pull_request) = previous_pull_request {
            derive_pull_request_events(
                repository,
                Some(previous_pull_request),
                current_pull_request,
                repository_comparison,
                identity_frontier,
                ids,
                &mut events,
            )?;
        } else if let Some(compacted) =
            merged_baselines_by_number.get(&current_pull_request.context().number())
        {
            derive_compacted_merged_pull_request_events(
                repository,
                compacted,
                current_pull_request,
                repository_comparison,
                identity_frontier,
                ids,
                &mut events,
            )?;
        } else {
            derive_pull_request_events(
                repository,
                None,
                current_pull_request,
                repository_comparison,
                identity_frontier,
                ids,
                &mut events,
            )?;
        }
    }
    if let Some(previous) = previous {
        derive_workflow_events(
            repository,
            previous.state(),
            current.state(),
            identity_frontier,
            ids,
            &mut events,
        )?;
    }
    Ok(events)
}

#[allow(clippy::too_many_arguments)]
fn derive_compacted_merged_pull_request_events(
    repository: &RepositorySlug,
    previous: &RepoWatchMergedPullRequestBaselineV1,
    current: &RepoWatchPullRequestState,
    repository_comparison: RepositoryComparison<'_>,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
) -> Result<(), RepoWatchDifferError> {
    let context = current.context();
    let opened_now = current.lifecycle() == RepoWatchPullRequestLifecycle::Open;
    if opened_now {
        push_pull_request_event(
            repository,
            context,
            RepoWatchEventKindV1::PullRequestOpened,
            RepoWatchEventStreamKeyV1::PullRequestKind {
                number: context.number(),
                kind: RepoWatchEventKindNameV1::PullRequestOpened,
            },
            identity_frontier,
            ids,
            events,
        )?;
        push_pull_request_event(
            repository,
            context,
            RepoWatchEventKindV1::MergeableStateChanged {
                current: current.mergeable_state(),
            },
            RepoWatchEventStreamKeyV1::PullRequestKind {
                number: context.number(),
                kind: RepoWatchEventKindNameV1::MergeableStateChanged,
            },
            identity_frontier,
            ids,
            events,
        )?;
    }
    if previous.head_sha() != context.head_sha() {
        push_pull_request_event(
            repository,
            context,
            RepoWatchEventKindV1::HeadChanged {
                previous: previous.head_sha().clone(),
                current: context.head_sha().clone(),
            },
            RepoWatchEventStreamKeyV1::PullRequestKind {
                number: context.number(),
                kind: RepoWatchEventKindNameV1::HeadChanged,
            },
            identity_frontier,
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
            RepoWatchEventStreamKeyV1::PullRequestKind {
                number: context.number(),
                kind: RepoWatchEventKindNameV1::MergeableStateChanged,
            },
            identity_frontier,
            ids,
            events,
        )?;
    }
    for suite in current.completed_check_suites() {
        if !previous.completed_check_suites().iter().any(|prior| {
            prior.id() == suite.id()
                && prior.completion_generation() == suite.completion_generation()
        }) {
            push_pull_request_event(
                repository,
                context,
                RepoWatchEventKindV1::ChecksCompleted {
                    outcome: suite.outcome(),
                },
                RepoWatchEventStreamKeyV1::CheckSuite {
                    number: context.number(),
                    suite: suite.id(),
                    completion_generation: suite.completion_generation(),
                },
                identity_frontier,
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
                context,
                RepoWatchEventKindV1::CheckRunCompleted {
                    name: run.name().clone(),
                    conclusion: run.conclusion(),
                },
                RepoWatchEventStreamKeyV1::CheckRun {
                    number: context.number(),
                    run: run.id(),
                    completion_generation: run.completion_generation(),
                },
                identity_frontier,
                ids,
                events,
            )?;
        }
    }
    for review in current.reviews() {
        if !previous.review_ids().contains(&review.id())
            && let Some(state) = review.state()
        {
            push_pull_request_event(
                repository,
                context,
                RepoWatchEventKindV1::ReviewSubmitted {
                    reviewer: review.reviewer().clone(),
                    state,
                    commit: review.commit().clone(),
                },
                RepoWatchEventStreamKeyV1::Review {
                    number: context.number(),
                    review: review.id(),
                },
                identity_frontier,
                ids,
                events,
            )?;
        }
    }
    derive_compacted_thread_events(
        repository,
        previous,
        current,
        identity_frontier,
        ids,
        events,
    )?;
    derive_compacted_label_events(
        repository,
        previous,
        current,
        identity_frontier,
        ids,
        events,
    )?;
    if let Some(previous_repository) = repository_comparison.previous {
        derive_base_advanced_event(
            repository,
            previous_repository,
            repository_comparison.current,
            current,
            identity_frontier,
            ids,
            events,
        )?;
    }
    if previous.signal_reviewers() == repository_comparison.current_signal_reviewers {
        derive_compacted_reaction_events(
            repository,
            previous,
            current,
            identity_frontier,
            ids,
            events,
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct RepositoryComparison<'a> {
    previous: Option<&'a RepoWatchRepositoryState>,
    current: &'a RepoWatchRepositoryState,
    current_signal_reviewers: &'a [RepoWatchAuthorLogin],
    reaction_filter_unchanged: bool,
}

fn derive_pull_request_events(
    repository: &RepositorySlug,
    previous: Option<&RepoWatchPullRequestState>,
    current: &RepoWatchPullRequestState,
    repository_comparison: RepositoryComparison<'_>,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
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
                RepoWatchEventStreamKeyV1::PullRequestKind {
                    number: context.number(),
                    kind: RepoWatchEventKindNameV1::PullRequestOpened,
                },
                identity_frontier,
                ids,
                events,
            )?;
        }
        (Some(RepoWatchPullRequestLifecycle::Open), RepoWatchPullRequestLifecycle::Closed) => {
            push_pull_request_event(
                repository,
                context,
                RepoWatchEventKindV1::PullRequestClosed,
                RepoWatchEventStreamKeyV1::PullRequestKind {
                    number: context.number(),
                    kind: RepoWatchEventKindNameV1::PullRequestClosed,
                },
                identity_frontier,
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
                RepoWatchEventStreamKeyV1::PullRequestKind {
                    number: context.number(),
                    kind: RepoWatchEventKindNameV1::PullRequestMerged,
                },
                identity_frontier,
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
            RepoWatchEventStreamKeyV1::PullRequestKind {
                number: context.number(),
                kind: RepoWatchEventKindNameV1::MergeableStateChanged,
            },
            identity_frontier,
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
                identity_frontier,
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
            RepoWatchEventStreamKeyV1::PullRequestKind {
                number: context.number(),
                kind: RepoWatchEventKindNameV1::HeadChanged,
            },
            identity_frontier,
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
            RepoWatchEventStreamKeyV1::PullRequestKind {
                number: context.number(),
                kind: RepoWatchEventKindNameV1::MergeableStateChanged,
            },
            identity_frontier,
            ids,
            events,
        )?;
    }
    derive_check_events(
        repository,
        previous,
        current,
        identity_frontier,
        ids,
        events,
    )?;
    derive_review_events(
        repository,
        previous,
        current,
        identity_frontier,
        ids,
        events,
    )?;
    derive_thread_events(
        repository,
        previous,
        current,
        identity_frontier,
        ids,
        events,
    )?;
    derive_label_events(
        repository,
        previous,
        current,
        identity_frontier,
        ids,
        events,
    )?;
    if let Some(previous_repository) = repository_comparison.previous {
        derive_base_advanced_event(
            repository,
            previous_repository,
            repository_comparison.current,
            current,
            identity_frontier,
            ids,
            events,
        )?;
    }
    if repository_comparison.reaction_filter_unchanged {
        derive_reaction_events(
            repository,
            previous,
            current,
            identity_frontier,
            ids,
            events,
        )?;
    }
    Ok(())
}

fn derive_check_events(
    repository: &RepositorySlug,
    previous: &RepoWatchPullRequestState,
    current: &RepoWatchPullRequestState,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
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
                RepoWatchEventStreamKeyV1::CheckSuite {
                    number: current.context().number(),
                    suite: suite.id(),
                    completion_generation: suite.completion_generation(),
                },
                identity_frontier,
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
                RepoWatchEventStreamKeyV1::CheckRun {
                    number: current.context().number(),
                    run: run.id(),
                    completion_generation: run.completion_generation(),
                },
                identity_frontier,
                ids,
                events,
            )?;
        }
    }
    Ok(())
}

fn derive_compacted_thread_events(
    repository: &RepositorySlug,
    previous: &RepoWatchMergedPullRequestBaselineV1,
    current: &RepoWatchPullRequestState,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
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
                RepoWatchEventStreamKeyV1::Thread {
                    number: current.context().number(),
                    kind: RepoWatchEventKindNameV1::ThreadOpened,
                    thread: thread.thread(),
                },
                identity_frontier,
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
                    RepoWatchEventStreamKeyV1::Thread {
                        number: current.context().number(),
                        kind: RepoWatchEventKindNameV1::ThreadResolved,
                        thread: thread.thread(),
                    },
                    identity_frontier,
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
                    RepoWatchEventStreamKeyV1::Thread {
                        number: current.context().number(),
                        kind: RepoWatchEventKindNameV1::ThreadOpened,
                        thread: thread.thread(),
                    },
                    identity_frontier,
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

fn derive_compacted_label_events(
    repository: &RepositorySlug,
    previous: &RepoWatchMergedPullRequestBaselineV1,
    current: &RepoWatchPullRequestState,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
) -> Result<(), RepoWatchDifferError> {
    for label in current.context().labels() {
        if !previous.labels().contains(label) {
            push_pull_request_event(
                repository,
                current.context(),
                RepoWatchEventKindV1::Labeled {
                    label: label.clone(),
                },
                RepoWatchEventStreamKeyV1::Label {
                    number: current.context().number(),
                    kind: RepoWatchEventKindNameV1::Labeled,
                    label,
                },
                identity_frontier,
                ids,
                events,
            )?;
        }
    }
    for label in previous.labels() {
        if !current.context().labels().contains(label) {
            push_pull_request_event(
                repository,
                current.context(),
                RepoWatchEventKindV1::Unlabeled {
                    label: label.clone(),
                },
                RepoWatchEventStreamKeyV1::Label {
                    number: current.context().number(),
                    kind: RepoWatchEventKindNameV1::Unlabeled,
                    label,
                },
                identity_frontier,
                ids,
                events,
            )?;
        }
    }
    Ok(())
}

fn derive_compacted_reaction_events(
    repository: &RepositorySlug,
    previous: &RepoWatchMergedPullRequestBaselineV1,
    current: &RepoWatchPullRequestState,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
) -> Result<(), RepoWatchDifferError> {
    for reaction in current.reactions() {
        if !previous.reactions().contains(reaction) {
            push_reaction_event(
                repository,
                current,
                reaction,
                ReactionChange::Added,
                identity_frontier,
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
                identity_frontier,
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
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
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
                RepoWatchEventStreamKeyV1::Review {
                    number: current.context().number(),
                    review: review.id(),
                },
                identity_frontier,
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
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
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
                RepoWatchEventStreamKeyV1::Thread {
                    number: current.context().number(),
                    kind: RepoWatchEventKindNameV1::ThreadOpened,
                    thread: thread.thread(),
                },
                identity_frontier,
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
                    RepoWatchEventStreamKeyV1::Thread {
                        number: current.context().number(),
                        kind: RepoWatchEventKindNameV1::ThreadResolved,
                        thread: thread.thread(),
                    },
                    identity_frontier,
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
                    RepoWatchEventStreamKeyV1::Thread {
                        number: current.context().number(),
                        kind: RepoWatchEventKindNameV1::ThreadOpened,
                        thread: thread.thread(),
                    },
                    identity_frontier,
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
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
) -> Result<(), RepoWatchDifferError> {
    for label in current.context().labels() {
        if !previous.context().labels().contains(label) {
            push_pull_request_event(
                repository,
                current.context(),
                RepoWatchEventKindV1::Labeled {
                    label: label.clone(),
                },
                RepoWatchEventStreamKeyV1::Label {
                    number: current.context().number(),
                    kind: RepoWatchEventKindNameV1::Labeled,
                    label,
                },
                identity_frontier,
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
                RepoWatchEventStreamKeyV1::Label {
                    number: current.context().number(),
                    kind: RepoWatchEventKindNameV1::Unlabeled,
                    label,
                },
                identity_frontier,
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
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
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
            RepoWatchEventStreamKeyV1::BaseAdvance {
                number: current_pull_request.context().number(),
                branch,
            },
            identity_frontier,
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
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
) -> Result<(), RepoWatchDifferError> {
    for reaction in current.reactions() {
        if !previous.reactions().contains(reaction) {
            push_reaction_event(
                repository,
                current,
                reaction,
                ReactionChange::Added,
                identity_frontier,
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
                identity_frontier,
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
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
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
        RepoWatchEventStreamKeyV1::Reaction {
            number: current.context().number(),
            subject: reaction.subject(),
            reactor: reaction.reactor(),
            content: reaction.content(),
            change,
        },
        identity_frontier,
        ids,
        events,
    )
}

fn derive_workflow_events(
    repository: &RepositorySlug,
    previous: &RepoWatchRepositoryState,
    current: &RepoWatchRepositoryState,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
) -> Result<(), RepoWatchDifferError> {
    for run in current.workflow_runs() {
        let already_observed = previous.workflow_runs().iter().any(|prior| {
            prior.branch() == run.branch()
                && prior.id() == run.id()
                && prior.attempt() == run.attempt()
        });
        if !already_observed {
            let event = RepoWatchEvent::branch_workflow(
                ids.next_event_id(),
                repository.clone(),
                run.branch().clone(),
                run.workflow().clone(),
                run.conclusion(),
            );
            push_identified_event(
                event,
                RepoWatchEventStreamKeyV1::Workflow {
                    branch: run.branch(),
                    workflow: run.workflow_id(),
                    run: run.id(),
                    attempt: run.attempt(),
                },
                identity_frontier,
                events,
            )?;
        }
    }
    Ok(())
}

fn push_pull_request_event(
    repository: &RepositorySlug,
    context: &PullRequestEventContext,
    kind: RepoWatchEventKindV1,
    stream_key: RepoWatchEventStreamKeyV1<'_>,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    ids: &mut impl RepoWatchEventIdGenerator,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
) -> Result<(), RepoWatchDifferError> {
    let event = RepoWatchEvent::try_pull_request(
        ids.next_event_id(),
        repository.clone(),
        context.clone(),
        kind,
    )
    .map_err(RepoWatchDifferError::from)?;
    push_identified_event(event, stream_key, identity_frontier, events)
}

fn push_identified_event(
    event: RepoWatchEvent,
    stream_key: RepoWatchEventStreamKeyV1<'_>,
    identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
    events: &mut Vec<RepoWatchEventOccurrenceV1>,
) -> Result<(), RepoWatchDifferError> {
    let is_recurring = stream_key.is_recurring();
    let pull_request_number = stream_key.pull_request_number();
    let stream_identity = repo_watch_event_stream_identity_v1(stream_key);
    let sequence = if is_recurring {
        identity_frontier.advance(stream_identity, pull_request_number)?
    } else {
        NonZeroU64::MIN
    };
    let content_identity = repo_watch_event_content_identity_v1(&event, stream_identity, sequence);
    events.push(RepoWatchEventOccurrenceV1 {
        event,
        content_identity,
    });
    Ok(())
}

struct RepoWatchIdentityHasher(Sha256);

impl RepoWatchIdentityHasher {
    fn new(domain: &[u8]) -> Self {
        let mut value = Self(Sha256::new());
        value.frame(domain);
        value
    }

    fn frame(&mut self, value: &[u8]) {
        self.0.update((value.len() as u64).to_be_bytes());
        self.0.update(value);
    }

    fn text(&mut self, value: &str) {
        self.frame(value.as_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.frame(&value.to_be_bytes());
    }

    fn boolean(&mut self, value: bool) {
        self.frame(&[u8::from(value)]);
    }

    fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

fn repo_watch_event_stream_identity_v1(key: RepoWatchEventStreamKeyV1<'_>) -> [u8; 32] {
    let mut hash = RepoWatchIdentityHasher::new(REPO_WATCH_EVENT_STREAM_IDENTITY_DOMAIN_V1);
    match key {
        RepoWatchEventStreamKeyV1::PullRequestKind { number, kind } => {
            hash.text("pull_request_kind");
            hash.u64(number.get());
            hash.text(repo_watch_event_kind_discriminator(kind));
        }
        RepoWatchEventStreamKeyV1::Label {
            number,
            kind,
            label,
        } => {
            hash.text("label");
            hash.u64(number.get());
            hash.text(repo_watch_event_kind_discriminator(kind));
            hash.text(label.as_str());
        }
        RepoWatchEventStreamKeyV1::CheckSuite {
            number,
            suite,
            completion_generation,
        } => {
            hash.text("check_suite");
            hash.u64(number.get());
            hash.u64(suite.get());
            hash.text(completion_generation.as_str());
        }
        RepoWatchEventStreamKeyV1::CheckRun {
            number,
            run,
            completion_generation,
        } => {
            hash.text("check_run");
            hash.u64(number.get());
            hash.u64(run.get());
            hash.text(completion_generation.as_str());
        }
        RepoWatchEventStreamKeyV1::Review { number, review } => {
            hash.text("review");
            hash.u64(number.get());
            hash.u64(review.get());
        }
        RepoWatchEventStreamKeyV1::Thread {
            number,
            kind,
            thread,
        } => {
            hash.text("thread");
            hash.u64(number.get());
            hash.text(repo_watch_event_kind_discriminator(kind));
            hash.text(thread.as_str());
        }
        RepoWatchEventStreamKeyV1::Workflow {
            branch,
            workflow,
            run,
            attempt,
        } => {
            hash.text("workflow");
            hash.text(branch.as_str());
            hash.u64(workflow.get());
            hash.u64(run.get());
            hash.u64(attempt.get());
        }
        RepoWatchEventStreamKeyV1::BaseAdvance { number, branch } => {
            hash.text("base_advance");
            hash.u64(number.get());
            hash.text(branch.as_str());
        }
        RepoWatchEventStreamKeyV1::Reaction {
            number,
            subject,
            reactor,
            content,
            change,
        } => {
            hash.text("reaction");
            hash.u64(number.get());
            hash_reaction_subject(&mut hash, subject);
            hash.text(reactor.as_str());
            hash.text(content.as_str());
            hash.text(reaction_change_discriminator(change));
        }
    }
    hash.finish()
}

/// Whether two events frame identical identifying content.
///
/// **This is not identity equality, and equal identities are its precondition.**
/// The identity digest frames this content and then the stream identity and the
/// occurrence sequence, so two occurrences of one recurring fact — a label
/// added, removed, and added again under an unchanged context — frame equal
/// identified content while carrying different identities. Deciding to coalesce
/// on this alone would discard a genuine later occurrence.
///
/// Its use is to confirm that two occurrences *already known to share an
/// identity* agree on the content that identity is derived from. Storage looks
/// an occurrence up by content identity first and only then asks this, which is
/// the order that makes the answer meaningful.
///
/// Both sides come from `hash_identified_content`, the same framing the identity
/// is computed over, so this cannot disagree with the digest about which members
/// identify a fact: the random `RepoWatchEventId` and a workflow's mutable
/// display name are excluded from both.
pub fn repo_watch_events_have_equal_identified_content(
    left: &RepoWatchEvent,
    right: &RepoWatchEvent,
) -> bool {
    repo_watch_event_identified_content_v1(left) == repo_watch_event_identified_content_v1(right)
}

fn repo_watch_event_identified_content_v1(event: &RepoWatchEvent) -> [u8; 32] {
    let mut hash = RepoWatchIdentityHasher::new(REPO_WATCH_EVENT_IDENTIFIED_CONTENT_DOMAIN_V1);
    hash_identified_content(&mut hash, event);
    hash.finish()
}

fn repo_watch_event_content_identity_v1(
    event: &RepoWatchEvent,
    stream_identity: [u8; 32],
    sequence: NonZeroU64,
) -> RepoWatchEventContentIdentityV1 {
    let mut hash = RepoWatchIdentityHasher::new(REPO_WATCH_EVENT_CONTENT_IDENTITY_DOMAIN_V1);
    hash_identified_content(&mut hash, event);
    hash.frame(&stream_identity);
    hash.u64(sequence.get());
    RepoWatchEventContentIdentityV1::from_bytes(hash.finish())
}

/// Frames every member of an event that identifies the fact it states.
fn hash_identified_content(hash: &mut RepoWatchIdentityHasher, event: &RepoWatchEvent) {
    hash.text(event.repository().as_str());
    hash.u64(1);
    match event.target() {
        RepoWatchEventTarget::PullRequest(context) => {
            hash.text("pull_request");
            hash.u64(context.number().get());
            hash.text(context.head_sha().as_str());
            hash.text(context.head_repository().as_str());
            hash.text(context.base_branch().as_str());
            hash.text(context.head_branch().as_str());
            hash.text(context.title().as_str());
            hash.text(context.body().as_str());
            hash.u64(context.labels().len() as u64);
            for label in context.labels() {
                hash.text(label.as_str());
            }
            hash.boolean(context.draft());
            match context.author() {
                Some(author) => {
                    hash.frame(&[1]);
                    hash.text(author.as_str());
                }
                None => hash.frame(&[0]),
            }
        }
        RepoWatchEventTarget::Branch => hash.text("branch"),
    }
    hash_event_kind(hash, event.kind());
}

fn hash_event_kind(hash: &mut RepoWatchIdentityHasher, kind: &RepoWatchEventKindV1) {
    hash.text(repo_watch_event_kind_discriminator(kind.name()));
    match kind {
        RepoWatchEventKindV1::PullRequestOpened
        | RepoWatchEventKindV1::PullRequestClosed
        | RepoWatchEventKindV1::PullRequestMerged => {}
        RepoWatchEventKindV1::HeadChanged { previous, current } => {
            hash.text(previous.as_str());
            hash.text(current.as_str());
        }
        RepoWatchEventKindV1::MergeableStateChanged { current } => {
            hash.text(mergeable_state_discriminator(*current));
        }
        RepoWatchEventKindV1::ChecksCompleted { outcome } => {
            hash.text(checks_outcome_discriminator(*outcome));
        }
        RepoWatchEventKindV1::CheckRunCompleted { name, conclusion } => {
            hash.text(name.as_str());
            hash.text(check_conclusion_discriminator(*conclusion));
        }
        // The workflow display name is deliberately excluded. It is
        // rule-visible payload, not an identifying member: the differ
        // suppresses a re-observed run attempt by branch, workflow identity,
        // run identity, and attempt, every one of which the stream identity
        // already names, and a provider can rename a workflow under all of
        // them. Hashing the name would mint a new identity for a run that
        // leaves the observation and returns after a rename, and commit
        // coalescing could no longer recognize the occurrence already durable
        // for it. Runs sharing a display name stay distinct through the
        // workflow identity in the stream.
        RepoWatchEventKindV1::BranchWorkflowRunCompleted {
            branch,
            workflow: _,
            conclusion,
        } => {
            hash.text(branch.as_str());
            hash.text(check_conclusion_discriminator(*conclusion));
        }
        RepoWatchEventKindV1::ReviewSubmitted {
            reviewer,
            state,
            commit,
        } => {
            hash.text(reviewer.as_str());
            hash.text(review_state_discriminator(*state));
            hash.text(commit.as_str());
        }
        RepoWatchEventKindV1::ThreadOpened { thread }
        | RepoWatchEventKindV1::ThreadResolved { thread } => hash.text(thread.as_str()),
        RepoWatchEventKindV1::Labeled { label } | RepoWatchEventKindV1::Unlabeled { label } => {
            hash.text(label.as_str());
        }
        RepoWatchEventKindV1::BaseAdvanced { branch } => hash.text(branch.as_str()),
        RepoWatchEventKindV1::ReactionChanged {
            subject,
            reactor,
            content,
            change,
        } => {
            hash_reaction_subject(hash, *subject);
            hash.text(reactor.as_str());
            hash.text(content.as_str());
            hash.text(reaction_change_discriminator(*change));
        }
    }
}

fn hash_reaction_subject(hash: &mut RepoWatchIdentityHasher, subject: ReactionSubject) {
    match subject {
        ReactionSubject::PullRequestBody => hash.text("pull_request_body"),
        ReactionSubject::IssueComment { id } => {
            hash.text("issue_comment");
            hash.u64(id.get());
        }
        ReactionSubject::ReviewComment { id } => {
            hash.text("review_comment");
            hash.u64(id.get());
        }
    }
}

const fn repo_watch_event_kind_discriminator(kind: RepoWatchEventKindNameV1) -> &'static str {
    match kind {
        RepoWatchEventKindNameV1::PullRequestOpened => "pull_request_opened",
        RepoWatchEventKindNameV1::PullRequestClosed => "pull_request_closed",
        RepoWatchEventKindNameV1::PullRequestMerged => "pull_request_merged",
        RepoWatchEventKindNameV1::HeadChanged => "head_changed",
        RepoWatchEventKindNameV1::MergeableStateChanged => "mergeable_state_changed",
        RepoWatchEventKindNameV1::ChecksCompleted => "checks_completed",
        RepoWatchEventKindNameV1::CheckRunCompleted => "check_run_completed",
        RepoWatchEventKindNameV1::BranchWorkflowRunCompleted => "branch_workflow_run_completed",
        RepoWatchEventKindNameV1::ReviewSubmitted => "review_submitted",
        RepoWatchEventKindNameV1::ThreadOpened => "thread_opened",
        RepoWatchEventKindNameV1::ThreadResolved => "thread_resolved",
        RepoWatchEventKindNameV1::Labeled => "labeled",
        RepoWatchEventKindNameV1::Unlabeled => "unlabeled",
        RepoWatchEventKindNameV1::BaseAdvanced => "base_advanced",
        RepoWatchEventKindNameV1::ReactionChanged => "reaction_changed",
    }
}

const fn mergeable_state_discriminator(value: MergeableState) -> &'static str {
    match value {
        MergeableState::Mergeable => "mergeable",
        MergeableState::Conflicting => "conflicting",
        MergeableState::Unknown => "unknown",
    }
}

const fn checks_outcome_discriminator(value: ChecksOutcome) -> &'static str {
    match value {
        ChecksOutcome::Success => "success",
        ChecksOutcome::Failure => "failure",
    }
}

const fn check_conclusion_discriminator(value: CheckConclusion) -> &'static str {
    match value {
        CheckConclusion::Success => "success",
        CheckConclusion::Failure => "failure",
        CheckConclusion::Neutral => "neutral",
        CheckConclusion::Cancelled => "cancelled",
        CheckConclusion::Skipped => "skipped",
        CheckConclusion::TimedOut => "timed_out",
        CheckConclusion::ActionRequired => "action_required",
        CheckConclusion::Stale => "stale",
        CheckConclusion::StartupFailure => "startup_failure",
    }
}

const fn review_state_discriminator(value: ReviewState) -> &'static str {
    match value {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes_requested",
        ReviewState::Commented => "commented",
    }
}

const fn reaction_change_discriminator(value: ReactionChange) -> &'static str {
    match value {
        ReactionChange::Added => "added",
        ReactionChange::Removed => "removed",
    }
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
        GoalUserCommand,
    ) {
        (
            self.action,
            self.prepared_session,
            self.initial_input,
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
        ids: &mut (impl SubmitInputIdGenerator + Send),
    ) -> impl Future<Output = Result<RepoWatchRuleEvaluationOutcome, Self::Error>> + Send;
}

/// Candidate identity supply for one repository-watch dispatch batch.
pub trait RepoWatchDispatchIdGenerator {
    fn next_dispatch_id(&mut self) -> RepoWatchDispatchId;
    fn next_command_id(&mut self) -> DurableCommandId;
    fn next_session_id(&mut self) -> SessionId;
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
}

impl SubmitInputIdGenerator for UuidV7RepoWatchDispatchIdGenerator {
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

    fn next_closure_decision_command_id(&mut self) -> DurableCommandId {
        DurableCommandId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_closure_turn_attempt_id(&mut self) -> TurnAttemptId {
        TurnAttemptId::from_uuid(uuid::Uuid::now_v7())
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
    Ids: RepoWatchDispatchIdGenerator + SubmitInputIdGenerator + Send,
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
                .handle_repo_watch_evaluation(
                    RepoWatchRuleEvaluation::NotMatched {
                        event,
                        rule_id: rule.id().clone(),
                        rule_version: rule.version(),
                    },
                    &mut self.ids,
                )
                .await
                .map_err(RepoWatchDispatchServiceError::Transaction);
        }
        let singleton = singleton_key(rule.singleton_per(), &event, observation)
            .ok_or(RepoWatchDispatchPreparationError::InvalidSingletonTarget)
            .map_err(RepoWatchDispatchServiceError::Preparation)?;
        let dispatch_id = self.ids.next_dispatch_id();
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
                SessionCreationProvenance::module_dispatched(ModuleDispatch::RepositoryWatch {
                    dispatch: dispatch_id,
                }),
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
            let goal = GoalUserCommand::new(
                self.ids.next_command_id(),
                session,
                GoalUserAction::Attach(statement),
            );
            prepared_actions.push(RepoWatchPreparedDispatchAction {
                action,
                prepared_session,
                initial_input,
                goal,
            });
        }
        self.transaction
            .handle_repo_watch_evaluation(
                RepoWatchRuleEvaluation::Matched {
                    dispatch_id,
                    event,
                    rule_id: rule.id().clone(),
                    rule_version: rule.version(),
                    singleton,
                    cooldown: rule.cooldown(),
                    actions: prepared_actions.into_boxed_slice(),
                },
                &mut self.ids,
            )
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
    const REPLACEMENT_REVIEWER: &str = "replacement-reviewer";
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
        settled: bool,
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
                settled: facts.settled,
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
            mergeable_state: MergeableState::Mergeable,
            settled: true,
            review_decision: RepoWatchReviewDecision::None,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
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
            settled: true,
            review_decision: RepoWatchReviewDecision::Approved,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
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
            settled: true,
            review_decision: RepoWatchReviewDecision::Approved,
            unresolved_threads: vec![ReviewThreadId::try_new(String::from(THREAD_ID))?],
            gating_check_count: 1,
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
            settled: true,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        })?;

        assert_eq!(
            assessment.verdict(),
            RepoWatchConvergenceVerdict::NotConverged
        );
        Ok(())
    }

    #[test]
    fn pending_mergeability_blocks_an_otherwise_green_head() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Unknown,
            settled: false,
            review_decision: RepoWatchReviewDecision::Approved,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        })?;

        assert_eq!(
            assessment.verdict(),
            RepoWatchConvergenceVerdict::NotConverged
        );
        Ok(())
    }

    #[test]
    fn missing_gating_checks_block_an_otherwise_green_head() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Mergeable,
            settled: true,
            review_decision: RepoWatchReviewDecision::Approved,
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
            settled: true,
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
            settled: true,
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
            settled: true,
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
            settled: true,
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
    fn unsettled_head_prevents_stale_review_clearance() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Mergeable,
            settled: false,
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

    /// A head with no gating check presents the same empty non-green list as a
    /// fully green one. The convergence rule already calls that head blocked,
    /// and clearance must agree: a settled head whose only stated blocker is
    /// the review, but which never ran a check, has no green evidence to
    /// dismiss the review against.
    #[test]
    fn zero_gating_checks_prevent_stale_review_clearance() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Mergeable,
            settled: true,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 0,
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
    fn merge_conflict_prevents_stale_review_clearance() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Conflicting,
            settled: true,
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

    /// The durable planner admits a clearance only against an assessment row
    /// carrying `mergeable_state = 'mergeable'`, so the in-memory rule must
    /// refuse `Unknown` too and not merely `Conflicting`. A settled head whose
    /// mergeability GitHub is still computing is the absence of evidence, and
    /// admitting it here would mint a candidate the durable planner refuses.
    #[test]
    fn unknown_mergeability_prevents_stale_review_clearance() -> Result<(), Box<dyn Error>> {
        let assessment = convergence_assessment(ConvergenceFacts {
            base_branch: BASE_BRANCH,
            mergeable_state: MergeableState::Unknown,
            settled: true,
            review_decision: RepoWatchReviewDecision::ChangesRequested,
            unresolved_threads: Vec::new(),
            gating_check_count: 1,
            non_green_gating_checks: Vec::new(),
        })?;
        assert_eq!(assessment.mergeable_state(), MergeableState::Unknown);
        assert!(assessment.settled());

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

    fn merged_baseline_input() -> Result<RepoWatchMergedPullRequestBaselineInputV1, Box<dyn Error>>
    {
        Ok(RepoWatchMergedPullRequestBaselineInputV1 {
            number: pull_request_number(PULL_REQUEST_NUMBER),
            head_sha: CommitSha::try_new(String::from(INITIAL_HEAD))?,
            signal_reviewers: vec![reviewer(REVIEWER)?],
            labels: Vec::new(),
            mergeable_state: MergeableState::Mergeable,
            completed_check_suites: Vec::new(),
            completed_check_runs: Vec::new(),
            review_ids: Vec::new(),
            threads: Vec::new(),
            reactions: Vec::new(),
        })
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
        let mut identity_frontier = RepoWatchEventIdentityFrontierV1::default();
        Ok(derive_repo_watch_events(
            &repository()?,
            previous,
            current,
            &mut identity_frontier,
            &mut FixedEventIds::new(),
        )?
        .into_iter()
        .map(RepoWatchEventOccurrenceV1::into_event)
        .collect())
    }

    fn derive_occurrences(
        previous: Option<&RepoWatchObservation>,
        current: &RepoWatchObservation,
        identity_frontier: &mut RepoWatchEventIdentityFrontierV1,
        first_event_id: u128,
    ) -> Result<Vec<RepoWatchEventOccurrenceV1>, Box<dyn Error>> {
        Ok(derive_repo_watch_events(
            &repository()?,
            previous,
            current,
            identity_frontier,
            &mut FixedEventIds {
                next: first_event_id,
            },
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

    /// Equal identified content is not identity equality. A label added,
    /// removed, and added again restates the first fact exactly, so the two
    /// occurrences frame equal content, and only the advancing occurrence
    /// sequence separates their identities. A caller that coalesced on content
    /// alone would discard the second addition.
    #[test]
    fn equal_identified_content_is_not_identity_equality() -> Result<(), Box<dyn Error>> {
        let ready = label(LABEL_READY)?;
        let without = observation(
            vec![pull_request(PullRequestFacts {
                labels: Vec::new(),
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let with = observation(
            vec![pull_request(PullRequestFacts {
                labels: vec![ready.clone()],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let mut frontier = RepoWatchEventIdentityFrontierV1::default();

        let added = derive_occurrences(Some(&without), &with, &mut frontier, 1)?;
        let removed = derive_occurrences(Some(&with), &without, &mut frontier, 10)?;
        let added_again = derive_occurrences(Some(&without), &with, &mut frontier, 20)?;

        assert_eq!(added.len(), 1);
        assert_eq!(removed.len(), 1);
        assert_eq!(added_again.len(), 1);
        assert!(repo_watch_events_have_equal_identified_content(
            added[0].event(),
            added_again[0].event()
        ));
        assert_ne!(
            added[0].content_identity(),
            added_again[0].content_identity(),
            "a repeated label must advance its occurrence sequence"
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
    fn compact_merged_baseline_preserves_post_merge_recurring_events() -> Result<(), Box<dyn Error>>
    {
        let merged = pull_request(PullRequestFacts {
            lifecycle: RepoWatchPullRequestLifecycle::Merged,
            ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
        })?;
        let baseline = RepoWatchMergedPullRequestBaselineV1::from_merged_state(
            &merged,
            &[reviewer(REVIEWER)?],
        )?
        .expect("merged fixture produces a compact baseline");
        let current_suite = RepoWatchCheckSuiteObservation::new(
            object_id(CHECK_SUITE_ID),
            completion_generation(CHECK_COMPLETION_GENERATION)?,
            ChecksOutcome::Failure,
        );
        let current_run = RepoWatchCheckRunObservation::new(
            object_id(CHECK_RUN_ID),
            completion_generation(CHECK_COMPLETION_GENERATION)?,
            CheckRunName::try_new(String::from(CHECK_NAME))?,
            CheckConclusion::TimedOut,
        );
        let current_review = RepoWatchReviewObservation::new(
            object_id(REVIEW_ID),
            reviewer(REVIEWER)?,
            Some(ReviewState::ChangesRequested),
            CommitSha::try_new(String::from(REVIEW_COMMIT))?,
        );
        let current_thread = RepoWatchThreadObservation::new(
            ReviewThreadId::try_new(String::from(THREAD_ID))?,
            RepoWatchThreadState::Resolved,
        );
        let current_reaction = reaction()?;
        let current_label = label(LABEL_READY)?;
        let current = observation(
            vec![pull_request(PullRequestFacts {
                lifecycle: RepoWatchPullRequestLifecycle::Merged,
                labels: vec![current_label.clone()],
                completed_check_suites: vec![current_suite.clone()],
                completed_check_runs: vec![current_run.clone()],
                reviews: vec![current_review.clone()],
                threads: vec![current_thread.clone()],
                reactions: vec![current_reaction.clone()],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            vec![reviewer(REVIEWER)?],
        )?;
        let previous = observation(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![reviewer(REVIEWER)?],
        )?;
        let mut identity_frontier = RepoWatchEventIdentityFrontierV1::default();

        let events = derive_repo_watch_events_with_merged_baselines(
            &repository()?,
            Some(&previous),
            &[baseline],
            &current,
            &mut identity_frontier,
            &mut FixedEventIds::new(),
        )?
        .into_iter()
        .map(RepoWatchEventOccurrenceV1::into_event)
        .collect::<Vec<_>>();

        assert_eq!(events.len(), 7);
        assert_eq!(
            events[0].kind(),
            &RepoWatchEventKindV1::ChecksCompleted {
                outcome: current_suite.outcome(),
            }
        );
        assert_eq!(
            events[1].kind(),
            &RepoWatchEventKindV1::CheckRunCompleted {
                name: current_run.name().clone(),
                conclusion: current_run.conclusion(),
            }
        );
        assert_eq!(
            events[2].kind(),
            &RepoWatchEventKindV1::ReviewSubmitted {
                reviewer: current_review.reviewer().clone(),
                state: ReviewState::ChangesRequested,
                commit: current_review.commit().clone(),
            }
        );
        assert_eq!(
            events[3].kind(),
            &RepoWatchEventKindV1::ThreadOpened {
                thread: current_thread.thread().clone(),
            }
        );
        assert_eq!(
            events[4].kind(),
            &RepoWatchEventKindV1::ThreadResolved {
                thread: current_thread.thread().clone(),
            }
        );
        assert_eq!(
            events[5].kind(),
            &RepoWatchEventKindV1::Labeled {
                label: current_label,
            }
        );
        assert_eq!(
            events[6].kind(),
            &RepoWatchEventKindV1::ReactionChanged {
                subject: current_reaction.subject(),
                reactor: current_reaction.reactor().clone(),
                content: current_reaction.content().clone(),
                change: ReactionChange::Added,
            }
        );
        Ok(())
    }

    #[test]
    fn compact_merged_baseline_rejects_duplicate_check_run_identities() -> Result<(), Box<dyn Error>>
    {
        let duplicate = object_id(CHECK_RUN_ID);
        let mut input = merged_baseline_input()?;
        input.completed_check_runs = vec![
            RepoWatchMergedCheckRunBaselineV1::new(
                duplicate,
                completion_generation(CHECK_COMPLETION_GENERATION)?,
                CheckConclusion::Success,
            ),
            RepoWatchMergedCheckRunBaselineV1::new(
                duplicate,
                completion_generation(NEXT_CHECK_COMPLETION_GENERATION)?,
                CheckConclusion::Failure,
            ),
        ];

        let result = RepoWatchMergedPullRequestBaselineV1::try_new(input);

        assert_eq!(
            result,
            Err(RepoWatchRepositoryStateError::DuplicateCheckRun(duplicate))
        );
        Ok(())
    }

    #[test]
    fn compact_merged_baseline_rejects_duplicate_check_suite_identities()
    -> Result<(), Box<dyn Error>> {
        let duplicate = object_id(CHECK_SUITE_ID);
        let mut input = merged_baseline_input()?;
        input.completed_check_suites = vec![
            RepoWatchMergedCheckSuiteBaselineV1::new(
                duplicate,
                completion_generation(CHECK_COMPLETION_GENERATION)?,
            ),
            RepoWatchMergedCheckSuiteBaselineV1::new(
                duplicate,
                completion_generation(NEXT_CHECK_COMPLETION_GENERATION)?,
            ),
        ];

        let result = RepoWatchMergedPullRequestBaselineV1::try_new(input);

        assert_eq!(
            result,
            Err(RepoWatchRepositoryStateError::DuplicateCheckSuite(
                duplicate
            ))
        );
        Ok(())
    }

    #[test]
    fn compact_merged_baseline_rejects_duplicate_thread_identities() -> Result<(), Box<dyn Error>> {
        let duplicate = ReviewThreadId::try_new(String::from(THREAD_ID))?;
        let mut input = merged_baseline_input()?;
        input.threads = vec![
            RepoWatchThreadObservation::new(duplicate.clone(), RepoWatchThreadState::Open),
            RepoWatchThreadObservation::new(duplicate.clone(), RepoWatchThreadState::Resolved),
        ];

        let result = RepoWatchMergedPullRequestBaselineV1::try_new(input);

        assert_eq!(
            result,
            Err(RepoWatchRepositoryStateError::DuplicateThread(duplicate))
        );
        Ok(())
    }

    #[test]
    fn compact_merged_baseline_excludes_reactions_from_non_signal_reviewers()
    -> Result<(), Box<dyn Error>> {
        let mut input = merged_baseline_input()?;
        input.signal_reviewers = vec![reviewer(REPLACEMENT_REVIEWER)?];
        input.reactions = vec![reaction()?];

        let baseline = RepoWatchMergedPullRequestBaselineV1::try_new(input)?;

        assert!(baseline.reactions().is_empty());
        Ok(())
    }

    #[test]
    fn compact_merged_baseline_duplicates_fail_before_derivation() -> Result<(), Box<dyn Error>> {
        let baseline = RepoWatchMergedPullRequestBaselineV1::try_new(merged_baseline_input()?)?;
        let current = observation(Vec::new(), Vec::new(), Vec::new(), Vec::new())?;
        let mut identity_frontier = RepoWatchEventIdentityFrontierV1::default();

        let error = derive_repo_watch_events_with_merged_baselines(
            &repository()?,
            None,
            &[baseline.clone(), baseline],
            &current,
            &mut identity_frontier,
            &mut FixedEventIds::new(),
        )
        .expect_err("duplicate compact subjects fail closed");

        assert_eq!(error.kind(), RepoWatchDifferFailureKind::BaselineCollection);
        assert_eq!(
            identity_frontier,
            RepoWatchEventIdentityFrontierV1::default()
        );
        Ok(())
    }

    #[test]
    fn compact_merged_reactions_rebaseline_when_the_reviewer_filter_changes()
    -> Result<(), Box<dyn Error>> {
        let merged = pull_request(PullRequestFacts {
            lifecycle: RepoWatchPullRequestLifecycle::Merged,
            reactions: vec![reaction()?],
            ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
        })?;
        let baseline = RepoWatchMergedPullRequestBaselineV1::from_merged_state(
            &merged,
            &[reviewer(REVIEWER)?],
        )?
        .expect("merged fixture produces a compact baseline");
        let current = observation(
            vec![merged],
            Vec::new(),
            Vec::new(),
            vec![reviewer(REPLACEMENT_REVIEWER)?],
        )?;
        let previous = observation(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![reviewer(REPLACEMENT_REVIEWER)?],
        )?;
        let mut identity_frontier = RepoWatchEventIdentityFrontierV1::default();

        let events = derive_repo_watch_events_with_merged_baselines(
            &repository()?,
            Some(&previous),
            &[baseline],
            &current,
            &mut identity_frontier,
            &mut FixedEventIds::new(),
        )?;

        assert!(events.is_empty());
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

    /// A completed run that leaves the observation and returns after its
    /// workflow is renamed is the same run: provider identities and attempt are
    /// unchanged, and the differ already suppresses re-emission on exactly those
    /// members. Its content identity has to survive the rename, or commit
    /// coalescing cannot recognize the occurrence already durable for it and the
    /// run is recorded and dispatched a second time.
    #[test]
    fn renamed_workflow_run_reappearance_restates_its_content_identity()
    -> Result<(), Box<dyn Error>> {
        let absent = observation(Vec::new(), Vec::new(), Vec::new(), Vec::new())?;
        let named = observation(
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
        let renamed = observation(
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
        let mut frontier = RepoWatchEventIdentityFrontierV1::default();

        let recorded = derive_occurrences(Some(&absent), &named, &mut frontier, 1)?;
        // The branch is deleted and recreated, and the workflow is renamed
        // while the run is out of the observation.
        let returned = derive_occurrences(Some(&absent), &renamed, &mut frontier, 10)?;

        assert_eq!(recorded.len(), 1);
        assert_eq!(returned.len(), 1);
        assert_eq!(
            recorded[0].content_identity(),
            returned[0].content_identity(),
            "a renamed workflow's reappearing run must restate its identity"
        );
        // The rename is still visible to rules through the event payload.
        assert_ne!(recorded[0].event().kind(), returned[0].event().kind());
        Ok(())
    }

    /// Storage coalesces on this equivalence, so it has to agree with the
    /// identity: a renamed workflow's reappearing run restates its identity and
    /// must therefore state the same fact, while a different conclusion is a
    /// different fact under both.
    #[test]
    fn equal_identified_content_agrees_with_the_identity_under_one_sequence()
    -> Result<(), Box<dyn Error>> {
        let absent = observation(Vec::new(), Vec::new(), Vec::new(), Vec::new())?;
        let named = observation(
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
        let renamed = observation(
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
        let failed = observation(
            Vec::new(),
            vec![workflow_run_for(
                WORKFLOW_RUN_ID,
                WORKFLOW_ID,
                WORKFLOW_NAME,
                CheckConclusion::Failure,
            )?],
            Vec::new(),
            Vec::new(),
        )?;
        let mut frontier = RepoWatchEventIdentityFrontierV1::default();

        let first = derive_occurrences(Some(&absent), &named, &mut frontier, 1)?;
        let after_rename = derive_occurrences(Some(&absent), &renamed, &mut frontier, 10)?;
        let after_failure = derive_occurrences(Some(&absent), &failed, &mut frontier, 20)?;

        assert!(repo_watch_events_have_equal_identified_content(
            first[0].event(),
            after_rename[0].event()
        ));
        assert_eq!(
            first[0].content_identity(),
            after_rename[0].content_identity()
        );
        assert!(!repo_watch_events_have_equal_identified_content(
            first[0].event(),
            after_failure[0].event()
        ));
        assert_ne!(
            first[0].content_identity(),
            after_failure[0].content_identity()
        );
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

    #[test]
    fn equal_normalized_occurrences_ignore_random_event_identity() -> Result<(), Box<dyn Error>> {
        let current = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let mut first_frontier = RepoWatchEventIdentityFrontierV1::default();
        let mut second_frontier = RepoWatchEventIdentityFrontierV1::default();

        let first = derive_occurrences(None, &current, &mut first_frontier, 1)?;
        let second = derive_occurrences(None, &current, &mut second_frontier, 100)?;

        assert_ne!(first[0].event().id(), second[0].event().id());
        assert_eq!(first[0].content_identity(), second[0].content_identity());
        assert_eq!(first[1].content_identity(), second[1].content_identity());
        Ok(())
    }

    #[test]
    fn equal_later_transition_advances_its_content_identity() -> Result<(), Box<dyn Error>> {
        let without_label = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let with_label = observation(
            vec![pull_request(PullRequestFacts {
                labels: vec![label(LABEL_READY)?],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let mut frontier = RepoWatchEventIdentityFrontierV1::default();

        let first = derive_occurrences(Some(&without_label), &with_label, &mut frontier, 1)?;
        let removed = derive_occurrences(Some(&with_label), &without_label, &mut frontier, 2)?;
        let repeated = derive_occurrences(Some(&without_label), &with_label, &mut frontier, 3)?;

        assert_eq!(first.len(), 1);
        assert_eq!(removed.len(), 1);
        assert_eq!(repeated.len(), 1);
        assert_ne!(first[0].content_identity(), repeated[0].content_identity());
        Ok(())
    }

    #[test]
    fn provider_keyed_check_suites_have_distinct_content_identities() -> Result<(), Box<dyn Error>>
    {
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
                completed_check_suites: vec![
                    RepoWatchCheckSuiteObservation::new(
                        object_id(CHECK_SUITE_ID),
                        completion_generation(CHECK_COMPLETION_GENERATION)?,
                        ChecksOutcome::Success,
                    ),
                    RepoWatchCheckSuiteObservation::new(
                        object_id(CHECK_SUITE_ID + 1),
                        completion_generation(CHECK_COMPLETION_GENERATION)?,
                        ChecksOutcome::Success,
                    ),
                ],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let mut frontier = RepoWatchEventIdentityFrontierV1::default();

        let events = derive_occurrences(Some(&previous), &current, &mut frontier, 1)?;

        assert_eq!(events.len(), 2);
        assert_ne!(events[0].content_identity(), events[1].content_identity());
        assert_eq!(frontier.entries().len(), 0);
        Ok(())
    }

    #[test]
    fn opened_event_content_identity_has_a_stable_v1_fixture() -> Result<(), Box<dyn Error>> {
        let current = observation(
            vec![pull_request(PullRequestFacts::matching(
                PULL_REQUEST_NUMBER,
            ))?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        let mut frontier = RepoWatchEventIdentityFrontierV1::default();

        let events = derive_occurrences(None, &current, &mut frontier, 1)?;

        assert_eq!(
            events[0].content_identity().as_bytes(),
            &[
                223, 185, 48, 253, 187, 59, 190, 103, 33, 168, 251, 14, 111, 116, 146, 49, 83, 238,
                232, 90, 92, 56, 217, 66, 135, 45, 24, 220, 226, 228, 105, 30,
            ]
        );
        Ok(())
    }

    fn stream_identity_for(index: usize) -> [u8; 32] {
        let mut identity = [0_u8; 32];
        identity[..8].copy_from_slice(&(index as u64).to_be_bytes());
        identity
    }

    fn distinct_frontier_entries(count: usize) -> Vec<RepoWatchEventIdentityFrontierEntryV1> {
        (0..count)
            .map(|index| {
                RepoWatchEventIdentityFrontierEntryV1::new(
                    stream_identity_for(index),
                    NonZeroU64::MIN,
                )
            })
            .collect()
    }

    /// A completed check run edited back to an earlier conclusion restates that
    /// conclusion's facts exactly. Its occurrence sequence has to advance, or the
    /// restored conclusion carries the first event's content identity and commit
    /// coalescing drops it, announcing no event and dispatching no work.
    #[test]
    fn check_run_edited_back_to_an_earlier_conclusion_keeps_a_distinct_identity()
    -> Result<(), Box<dyn Error>> {
        let absent = observation_without_check_run()?;
        let failed = observation_with_completed_check_run(CheckConclusion::Failure)?;
        let succeeded = observation_with_completed_check_run(CheckConclusion::Success)?;
        let mut frontier = RepoWatchEventIdentityFrontierV1::default();

        let first = derive_occurrences(Some(&absent), &failed, &mut frontier, 1)?;
        let edited = derive_occurrences(Some(&failed), &succeeded, &mut frontier, 10)?;
        let restored = derive_occurrences(Some(&succeeded), &failed, &mut frontier, 20)?;

        assert_eq!(first.len(), 1);
        assert_eq!(edited.len(), 1);
        assert_eq!(restored.len(), 1);
        // The restored conclusion states the same facts as the first event, so
        // only the advancing occurrence sequence keeps their identities apart.
        assert_ne!(
            first[0].content_identity(),
            restored[0].content_identity(),
            "a restored check-run conclusion must not reuse the first event's identity"
        );
        assert_ne!(first[0].content_identity(), edited[0].content_identity());
        assert_ne!(edited[0].content_identity(), restored[0].content_identity());
        Ok(())
    }

    /// The pull request under test with no completed check run observed.
    fn observation_without_check_run() -> Result<RepoWatchObservation, Box<dyn Error>> {
        Ok(observation(
            vec![pull_request(PullRequestFacts {
                completed_check_runs: Vec::new(),
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?)
    }

    /// The pull request under test with one completed check run at this
    /// conclusion, keeping the run identity and completion generation fixed so
    /// only the conclusion distinguishes the observations.
    fn observation_with_completed_check_run(
        conclusion: CheckConclusion,
    ) -> Result<RepoWatchObservation, Box<dyn Error>> {
        Ok(observation(
            vec![pull_request(PullRequestFacts {
                completed_check_runs: vec![RepoWatchCheckRunObservation::new(
                    object_id(CHECK_RUN_ID),
                    completion_generation(CHECK_COMPLETION_GENERATION)?,
                    CheckRunName::try_new(String::from(CHECK_NAME))?,
                    conclusion,
                )],
                ..PullRequestFacts::matching(PULL_REQUEST_NUMBER)
            })?],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?)
    }

    #[test]
    fn identity_frontier_holds_exactly_the_stream_ceiling() -> Result<(), Box<dyn Error>> {
        let mut frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(
            distinct_frontier_entries(MAX_REPO_WATCH_EVENT_IDENTITY_STREAMS),
        )?;

        assert_eq!(
            frontier.entries().len(),
            MAX_REPO_WATCH_EVENT_IDENTITY_STREAMS
        );
        assert_eq!(frontier.advance(stream_identity_for(0), None)?.get(), 2);
        Ok(())
    }

    #[test]
    fn identity_frontier_rejects_one_stream_past_the_ceiling() {
        let entries = distinct_frontier_entries(MAX_REPO_WATCH_EVENT_IDENTITY_STREAMS + 1);

        assert_eq!(
            RepoWatchEventIdentityFrontierV1::try_from_entries(entries),
            Err(RepoWatchEventIdentityFrontierError::StreamLimit)
        );
    }

    #[test]
    fn identity_frontier_at_the_ceiling_refuses_an_unknown_stream() -> Result<(), Box<dyn Error>> {
        let mut frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(
            distinct_frontier_entries(MAX_REPO_WATCH_EVENT_IDENTITY_STREAMS),
        )?;

        assert_eq!(
            frontier.advance(
                stream_identity_for(MAX_REPO_WATCH_EVENT_IDENTITY_STREAMS),
                None
            ),
            Err(RepoWatchEventIdentityFrontierError::StreamLimit)
        );
        Ok(())
    }

    /// Ownership is the durable member a later retirement mechanism reads, so
    /// it has to survive the round trip the cursor performs on every commit.
    #[test]
    fn identity_frontier_entries_carry_their_owning_pull_request() -> Result<(), Box<dyn Error>> {
        let owning = pull_request_number(PULL_REQUEST_NUMBER);
        let frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
            RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
                stream_identity_for(0),
                NonZeroU64::MIN,
                owning,
            ),
            RepoWatchEventIdentityFrontierEntryV1::new(stream_identity_for(1), NonZeroU64::MIN),
        ])?;

        let entries = frontier.entries().collect::<Vec<_>>();
        assert_eq!(entries[0].pull_request_number(), Some(owning));
        assert_eq!(entries[1].pull_request_number(), None);
        Ok(())
    }
}
