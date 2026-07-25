//! Review-workflow aggregates above session execution.
//!
//! The normative specification is `docs/spec/review-workflows.md`.

use std::num::{NonZeroU32, NonZeroU64};

use crate::{
    AcceptedInputId, ContextFrontierId, ReviewExternalLinkId, ReviewFindingId, ReviewPassId,
    ReviewRunId, ReviewTargetId, SessionId, TurnId,
};

const REVIEW_KEY_MAXIMUM_BYTES: usize = 1_024;
const REVIEW_TEXT_MAXIMUM_BYTES: usize = 65_536;
const REVIEW_CONFIDENCE_MAXIMUM_BASIS_POINTS: u16 = 10_000;
const REVIEW_POLICY_VERSION_ONE_MINIMUM_JUDGE_BASIS_POINTS: u16 = 7_000;
const REVIEW_POLICY_VERSION_ONE_MINIMUM_PUBLICATION_BASIS_POINTS: u16 = 8_000;

/// Exact bounded text used for opaque provider, repository, revision, path, and external keys.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReviewKey(String);

impl ReviewKey {
    /// Checks a nonempty key without trimming or normalization.
    pub fn try_new(value: String) -> Result<Self, ReviewValueError> {
        validate_review_value(value, REVIEW_KEY_MAXIMUM_BYTES).map(Self)
    }

    /// Borrows the exact checked key.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact checked key.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Exact bounded narrative text used for finding content and reasons.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReviewText(String);

impl ReviewText {
    /// Checks nonempty text without trimming or normalization.
    pub fn try_new(value: String) -> Result<Self, ReviewValueError> {
        validate_review_value(value, REVIEW_TEXT_MAXIMUM_BYTES).map(Self)
    }

    /// Borrows the exact checked text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact checked text.
    pub fn into_string(self) -> String {
        self.0
    }
}

fn validate_review_value(value: String, maximum_bytes: usize) -> Result<String, ReviewValueError> {
    let failure = if value.is_empty() {
        Some(ReviewValueFailure::Empty)
    } else if value.contains('\0') {
        Some(ReviewValueFailure::ContainsNull)
    } else if value.len() > maximum_bytes {
        Some(ReviewValueFailure::TooLong { maximum_bytes })
    } else {
        None
    };

    match failure {
        Some(failure) => Err(ReviewValueError { value, failure }),
        None => Ok(value),
    }
}

/// Why an exact review-workflow string cannot be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewValueFailure {
    /// The string is empty.
    Empty,
    /// The string contains U+0000.
    ContainsNull,
    /// The UTF-8 representation exceeds the field's byte bound.
    TooLong {
        /// Maximum admitted UTF-8 byte count.
        maximum_bytes: usize,
    },
}

/// Failed string construction retaining the rejected value unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewValueError {
    value: String,
    failure: ReviewValueFailure,
}

impl ReviewValueError {
    /// Returns why the string was rejected.
    pub const fn failure(&self) -> ReviewValueFailure {
        self.failure
    }

    /// Borrows the rejected string.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the rejected string and failure.
    pub fn into_parts(self) -> (String, ReviewValueFailure) {
        (self.value, self.failure)
    }
}

/// A positive external change-request number.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewChangeRequestNumber(NonZeroU64);

impl ReviewChangeRequestNumber {
    /// Checks that `value` is positive.
    pub const fn try_new(value: u64) -> Result<Self, ReviewPositiveNumberError> {
        match NonZeroU64::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(ReviewPositiveNumberError),
        }
    }

    /// Returns the positive integer value.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// A one-based finding-event or external-observation ordinal.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewEventOrdinal(NonZeroU32);

impl ReviewEventOrdinal {
    /// Returns ordinal one.
    pub const fn one() -> Self {
        Self(NonZeroU32::MIN)
    }

    /// Checks that `value` is positive.
    pub const fn try_new(value: u32) -> Result<Self, ReviewPositiveNumberError> {
        match NonZeroU32::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(ReviewPositiveNumberError),
        }
    }

    /// Returns the one-based integer value.
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    fn checked_successor(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => NonZeroU32::new(value).map(Self),
            None => None,
        }
    }
}

/// A zero value where the review workflow requires a positive integer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPositiveNumberError;

/// Exact confidence in basis points.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewConfidence(u16);

impl ReviewConfidence {
    /// Checks a basis-point value from zero through 10,000.
    pub const fn try_from_basis_points(basis_points: u16) -> Result<Self, ReviewConfidenceError> {
        if basis_points <= REVIEW_CONFIDENCE_MAXIMUM_BASIS_POINTS {
            Ok(Self(basis_points))
        } else {
            Err(ReviewConfidenceError { basis_points })
        }
    }

    /// Returns the exact basis-point value.
    pub const fn basis_points(self) -> u16 {
        self.0
    }
}

/// Confidence outside the closed zero-through-10,000 basis-point range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewConfidenceError {
    basis_points: u16,
}

impl ReviewConfidenceError {
    /// Returns the rejected basis-point value.
    pub const fn basis_points(self) -> u16 {
        self.basis_points
    }
}

/// An ordinal review-policy version.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewPolicyVersion(NonZeroU32);

impl ReviewPolicyVersion {
    /// Returns version one.
    pub const fn one() -> Self {
        Self(NonZeroU32::MIN)
    }

    /// Checks that `value` is positive.
    pub const fn try_new(value: u32) -> Result<Self, ReviewPositiveNumberError> {
        match NonZeroU32::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(ReviewPositiveNumberError),
        }
    }

    /// Returns the positive integer version.
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Complete confidence policy frozen into one review run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPolicy {
    version: ReviewPolicyVersion,
    minimum_judge_confidence: ReviewConfidence,
    minimum_publication_confidence: ReviewConfidence,
}

impl ReviewPolicy {
    /// Constructs a policy, rejecting unordered thresholds or a noncanonical
    /// version-one tuple.
    pub const fn try_new(
        version: ReviewPolicyVersion,
        minimum_judge_confidence: ReviewConfidence,
        minimum_publication_confidence: ReviewConfidence,
    ) -> Result<Self, ReviewPolicyError> {
        let is_noncanonical_version_one = version.get() == ReviewPolicyVersion::one().get()
            && (minimum_judge_confidence.basis_points()
                != REVIEW_POLICY_VERSION_ONE_MINIMUM_JUDGE_BASIS_POINTS
                || minimum_publication_confidence.basis_points()
                    != REVIEW_POLICY_VERSION_ONE_MINIMUM_PUBLICATION_BASIS_POINTS);
        if is_noncanonical_version_one
            || minimum_publication_confidence.basis_points()
                < minimum_judge_confidence.basis_points()
        {
            Err(ReviewPolicyError {
                version,
                minimum_judge_confidence,
                minimum_publication_confidence,
            })
        } else {
            Ok(Self {
                version,
                minimum_judge_confidence,
                minimum_publication_confidence,
            })
        }
    }

    /// Returns the accepted version-one 70%/80% policy.
    pub const fn version_one() -> Self {
        Self {
            version: ReviewPolicyVersion::one(),
            minimum_judge_confidence: ReviewConfidence(
                REVIEW_POLICY_VERSION_ONE_MINIMUM_JUDGE_BASIS_POINTS,
            ),
            minimum_publication_confidence: ReviewConfidence(
                REVIEW_POLICY_VERSION_ONE_MINIMUM_PUBLICATION_BASIS_POINTS,
            ),
        }
    }

    /// Returns the policy version.
    pub const fn version(self) -> ReviewPolicyVersion {
        self.version
    }

    /// Returns the minimum confidence for judgment.
    pub const fn minimum_judge_confidence(self) -> ReviewConfidence {
        self.minimum_judge_confidence
    }

    /// Returns the minimum confidence for unattended publication.
    pub const fn minimum_publication_confidence(self) -> ReviewConfidence {
        self.minimum_publication_confidence
    }
}

/// A noncanonical or unordered review-policy tuple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPolicyError {
    version: ReviewPolicyVersion,
    minimum_judge_confidence: ReviewConfidence,
    minimum_publication_confidence: ReviewConfidence,
}

impl ReviewPolicyError {
    /// Returns the rejected complete policy tuple.
    pub const fn into_parts(self) -> (ReviewPolicyVersion, ReviewConfidence, ReviewConfidence) {
        (
            self.version,
            self.minimum_judge_confidence,
            self.minimum_publication_confidence,
        )
    }
}

/// What moving or immutable code-host subject a target snapshot represents.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewTargetSubject {
    /// One change request at the target's frozen head revision.
    ChangeRequest(ReviewChangeRequestNumber),
    /// One immutable commit revision.
    Commit,
}

/// One immutable review-target snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewTarget {
    id: ReviewTargetId,
    provider: ReviewKey,
    repository: ReviewKey,
    subject: ReviewTargetSubject,
    head_revision: ReviewKey,
    base_revision: Option<ReviewKey>,
    stack_parent: Option<ReviewTargetId>,
}

impl ReviewTarget {
    /// Constructs a target snapshot, rejecting a self-parent edge.
    pub fn try_new(
        id: ReviewTargetId,
        provider: ReviewKey,
        repository: ReviewKey,
        subject: ReviewTargetSubject,
        head_revision: ReviewKey,
        base_revision: Option<ReviewKey>,
        stack_parent: Option<ReviewTargetId>,
    ) -> Result<Self, ReviewTargetError> {
        if stack_parent == Some(id) {
            return Err(ReviewTargetError::SelfParent { target: id });
        }
        Ok(Self {
            id,
            provider,
            repository,
            subject,
            head_revision,
            base_revision,
            stack_parent,
        })
    }

    /// Returns the target identity.
    pub const fn id(&self) -> ReviewTargetId {
        self.id
    }

    /// Borrows the opaque code-host provider key.
    pub const fn provider(&self) -> &ReviewKey {
        &self.provider
    }

    /// Borrows the opaque repository key.
    pub const fn repository(&self) -> &ReviewKey {
        &self.repository
    }

    /// Returns the target subject.
    pub const fn subject(&self) -> ReviewTargetSubject {
        self.subject
    }

    /// Borrows the frozen head revision.
    pub const fn head_revision(&self) -> &ReviewKey {
        &self.head_revision
    }

    /// Borrows the optional frozen base revision.
    pub const fn base_revision(&self) -> Option<&ReviewKey> {
        self.base_revision.as_ref()
    }

    /// Returns the optional immediately preceding stack target.
    pub const fn stack_parent(&self) -> Option<ReviewTargetId> {
        self.stack_parent
    }
}

/// Invalid immutable target topology.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewTargetError {
    /// The target names itself as its stack parent.
    SelfParent {
        /// The self-parented target.
        target: ReviewTargetId,
    },
}

/// A target-bound review-run reference.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReviewRunRef {
    target: ReviewTargetId,
    run: ReviewRunId,
}

impl ReviewRunRef {
    /// Binds one run identity to its target.
    pub const fn new(target: ReviewTargetId, run: ReviewRunId) -> Self {
        Self { target, run }
    }

    /// Returns the target identity.
    pub const fn target(self) -> ReviewTargetId {
        self.target
    }

    /// Returns the run identity.
    pub const fn run(self) -> ReviewRunId {
        self.run
    }
}

/// A run-bound review-pass reference carrying complete ancestry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReviewPassRef {
    run: ReviewRunRef,
    pass: ReviewPassId,
}

impl ReviewPassRef {
    /// Binds one pass identity to its exact run.
    pub const fn new(run: ReviewRunRef, pass: ReviewPassId) -> Self {
        Self { run, pass }
    }

    /// Returns the run reference.
    pub const fn run(self) -> ReviewRunRef {
        self.run
    }

    /// Returns the pass identity.
    pub const fn pass(self) -> ReviewPassId {
        self.pass
    }

    /// Returns the target identity.
    pub const fn target(self) -> ReviewTargetId {
        self.run.target()
    }
}

/// A finding reference carrying complete target/run ancestry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReviewFindingRef {
    run: ReviewRunRef,
    finding: ReviewFindingId,
}

impl ReviewFindingRef {
    /// Binds one finding identity to its exact run.
    pub const fn new(run: ReviewRunRef, finding: ReviewFindingId) -> Self {
        Self { run, finding }
    }

    /// Returns the run reference.
    pub const fn run(self) -> ReviewRunRef {
        self.run
    }

    /// Returns the finding identity.
    pub const fn finding(self) -> ReviewFindingId {
        self.finding
    }

    /// Returns the target identity.
    pub const fn target(self) -> ReviewTargetId {
        self.run.target()
    }
}

/// One workflow operation represented by a review run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewWorkflowKind {
    /// Import current context from an external code host.
    ImportExternalContext,
    /// Produce findings without changing the reviewed workspace.
    ReadOnlyReview,
    /// Judge proposed findings.
    JudgeFindings,
    /// Deduplicate findings.
    DedupeFindings,
    /// Publish accepted findings externally.
    PublishReview,
    /// Attempt repairs for accepted findings.
    FixFindings,
    /// Propagate a merged stack edge.
    PropagateStack,
}

/// Evidence-bearing review-run state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewRunState {
    /// No pass is active yet.
    Queued,
    /// The named pass is active.
    Running {
        /// Exact active pass.
        active_pass: ReviewPassRef,
    },
    /// The named pass concluded the run successfully.
    Succeeded {
        /// Exact concluding pass.
        concluding_pass: ReviewPassRef,
    },
    /// The named pass concluded the run unsuccessfully.
    Failed {
        /// Exact failing pass.
        failed_pass: ReviewPassRef,
    },
    /// The named pass requires external resolution.
    Blocked {
        /// Exact blocking pass.
        blocking_pass: ReviewPassRef,
    },
    /// The run was cancelled, optionally after a recorded pass.
    Cancelled {
        /// Last pass known to the run, when one exists.
        last_pass: Option<ReviewPassRef>,
    },
}

impl ReviewRunState {
    fn pass(self) -> Option<ReviewPassRef> {
        match self {
            Self::Queued => None,
            Self::Running { active_pass } => Some(active_pass),
            Self::Succeeded { concluding_pass } => Some(concluding_pass),
            Self::Failed { failed_pass } => Some(failed_pass),
            Self::Blocked { blocking_pass } => Some(blocking_pass),
            Self::Cancelled { last_pass } => last_pass,
        }
    }
}

/// One review workflow execution against a frozen target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRun {
    reference: ReviewRunRef,
    workflow: ReviewWorkflowKind,
    policy: ReviewPolicy,
    state: ReviewRunState,
}

impl ReviewRun {
    /// Constructs a queued review run.
    pub const fn new(
        reference: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        policy: ReviewPolicy,
    ) -> Self {
        Self {
            reference,
            workflow,
            policy,
            state: ReviewRunState::Queued,
        }
    }

    /// Reconstitutes a run after validating its evidence reference.
    pub fn try_reconstitute(
        reference: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        policy: ReviewPolicy,
        state: ReviewRunState,
    ) -> Result<Self, ReviewRunTransitionError> {
        validate_run_state(reference, state).map_err(|failure| ReviewRunTransitionError {
            current: state,
            next: state,
            failure,
        })?;
        Ok(Self {
            reference,
            workflow,
            policy,
            state,
        })
    }

    /// Applies one permitted state transition.
    pub fn transition(mut self, next: ReviewRunState) -> Result<Self, ReviewRunTransitionError> {
        validate_run_state(self.reference, next).map_err(|failure| ReviewRunTransitionError {
            current: self.state,
            next,
            failure,
        })?;
        let permitted = match (self.state, next) {
            (ReviewRunState::Queued, ReviewRunState::Running { .. }) => true,
            (ReviewRunState::Queued, ReviewRunState::Cancelled { last_pass: None }) => true,
            (
                ReviewRunState::Running {
                    active_pass: current,
                },
                ReviewRunState::Succeeded {
                    concluding_pass: next,
                }
                | ReviewRunState::Failed { failed_pass: next }
                | ReviewRunState::Blocked {
                    blocking_pass: next,
                },
            ) => current == next,
            (
                ReviewRunState::Running {
                    active_pass: current,
                },
                ReviewRunState::Cancelled {
                    last_pass: Some(next),
                },
            ) => current == next,
            _ => false,
        };
        if !permitted {
            return Err(ReviewRunTransitionError {
                current: self.state,
                next,
                failure: ReviewRunTransitionFailure::InvalidTransition,
            });
        }
        self.state = next;
        Ok(self)
    }

    /// Returns the complete run reference.
    pub const fn reference(&self) -> ReviewRunRef {
        self.reference
    }

    /// Returns the workflow kind.
    pub const fn workflow(&self) -> ReviewWorkflowKind {
        self.workflow
    }

    /// Returns the frozen policy.
    pub const fn policy(&self) -> ReviewPolicy {
        self.policy
    }

    /// Returns the current state.
    pub const fn state(&self) -> ReviewRunState {
        self.state
    }
}

fn validate_run_state(
    reference: ReviewRunRef,
    state: ReviewRunState,
) -> Result<(), ReviewRunTransitionFailure> {
    if let Some(pass) = state.pass()
        && pass.run() != reference
    {
        return Err(ReviewRunTransitionFailure::ForeignPass);
    }
    Ok(())
}

/// Why a review-run state cannot be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewRunTransitionFailure {
    /// The state's pass does not belong to the run.
    ForeignPass,
    /// The requested lifecycle edge is not permitted.
    InvalidTransition,
}

/// Rejected run transition retaining both states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewRunTransitionError {
    current: ReviewRunState,
    next: ReviewRunState,
    failure: ReviewRunTransitionFailure,
}

impl ReviewRunTransitionError {
    /// Returns why the state was rejected.
    pub const fn failure(self) -> ReviewRunTransitionFailure {
        self.failure
    }

    /// Returns the current and requested states.
    pub const fn states(self) -> (ReviewRunState, ReviewRunState) {
        (self.current, self.next)
    }
}

/// One pass purpose inside a review run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewPassKind {
    /// Import external review context.
    ImportExternalContext,
    /// Produce read-only findings.
    ReadOnlyReview,
    /// Judge findings.
    Judge,
    /// Deduplicate findings.
    Dedupe,
    /// Publish findings externally.
    Publish,
    /// Attempt finding repairs.
    Fix,
    /// Propagate one stack edge.
    PropagateStack,
}

/// Evidence-bearing state of one session-backed review pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassState {
    /// The pass input is accepted but no turn is active yet.
    Queued,
    /// The named turn is active.
    Running {
        /// Exact pass turn.
        turn: TurnId,
    },
    /// The named turn succeeded at the exact output frontier.
    Succeeded {
        /// Exact pass turn.
        turn: TurnId,
        /// Exact terminal output frontier.
        output_frontier: ContextFrontierId,
    },
    /// The named turn failed.
    Failed {
        /// Exact pass turn.
        turn: TurnId,
    },
    /// The named turn requires external resolution.
    Blocked {
        /// Exact pass turn.
        turn: TurnId,
    },
    /// The pass was cancelled before or during its turn.
    Cancelled {
        /// Exact pass turn when activation occurred.
        turn: Option<TurnId>,
    },
}

impl ReviewPassState {
    fn turn(self) -> Option<TurnId> {
        match self {
            Self::Queued | Self::Cancelled { turn: None } => None,
            Self::Running { turn }
            | Self::Succeeded { turn, .. }
            | Self::Failed { turn }
            | Self::Blocked { turn }
            | Self::Cancelled { turn: Some(turn) } => Some(turn),
        }
    }
}

/// Canonical turn facts independently loaded for one review pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPassTurnEvidence {
    turn: TurnId,
    session: SessionId,
    accepted_input: AcceptedInputId,
    terminal_frontier: Option<ContextFrontierId>,
}

impl ReviewPassTurnEvidence {
    /// Supplies the turn's independently stored ownership and terminal facts.
    pub const fn new(
        turn: TurnId,
        session: SessionId,
        accepted_input: AcceptedInputId,
        terminal_frontier: Option<ContextFrontierId>,
    ) -> Self {
        Self {
            turn,
            session,
            accepted_input,
            terminal_frontier,
        }
    }

    /// Returns the canonical turn identity.
    pub const fn turn(self) -> TurnId {
        self.turn
    }

    /// Returns the canonical turn session.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the canonical turn-origin input.
    pub const fn accepted_input(self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the canonical terminal frontier, when the turn is terminal.
    pub const fn terminal_frontier(self) -> Option<ContextFrontierId> {
        self.terminal_frontier
    }
}

/// Complete independently stored facts for review-pass reconstitution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPassReconstitutionInput {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    session: SessionId,
    accepted_input: AcceptedInputId,
    accepted_input_session: SessionId,
    state: ReviewPassState,
    turn_evidence: Option<ReviewPassTurnEvidence>,
}

impl ReviewPassReconstitutionInput {
    /// Supplies the pass row plus canonical accepted-input and turn evidence.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        session: SessionId,
        accepted_input: AcceptedInputId,
        accepted_input_session: SessionId,
        state: ReviewPassState,
        turn_evidence: Option<ReviewPassTurnEvidence>,
    ) -> Self {
        Self {
            reference,
            kind,
            session,
            accepted_input,
            accepted_input_session,
            state,
            turn_evidence,
        }
    }

    /// Returns the complete pass reference.
    pub const fn reference(self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the pass kind.
    pub const fn kind(self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the session stored on the pass.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the accepted input stored on the pass.
    pub const fn accepted_input(self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the accepted input's canonical session.
    pub const fn accepted_input_session(self) -> SessionId {
        self.accepted_input_session
    }

    /// Returns the stored pass state.
    pub const fn state(self) -> ReviewPassState {
        self.state
    }

    /// Returns the canonical turn evidence, when the pass names a turn.
    pub const fn turn_evidence(self) -> Option<ReviewPassTurnEvidence> {
        self.turn_evidence
    }
}

/// One session-backed pass inside a review run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPass {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    session: SessionId,
    accepted_input: AcceptedInputId,
    state: ReviewPassState,
}

impl ReviewPass {
    /// Constructs a queued pass after its orchestration input is accepted.
    pub fn try_new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        session: SessionId,
        accepted_input: AcceptedInputId,
        accepted_input_session: SessionId,
    ) -> Result<Self, ReviewPassConstructionError> {
        if session != accepted_input_session {
            return Err(ReviewPassConstructionError {
                reference,
                kind,
                session,
                accepted_input,
                accepted_input_session,
            });
        }
        Ok(Self {
            reference,
            kind,
            session,
            accepted_input,
            state: ReviewPassState::Queued,
        })
    }

    /// Reconstitutes a pass after validating independently stored evidence.
    pub fn try_reconstitute(
        input: ReviewPassReconstitutionInput,
    ) -> Result<Self, ReviewPassReconstitutionError> {
        let failure = validate_pass_reconstitution(input);
        if let Some(failure) = failure {
            return Err(ReviewPassReconstitutionError {
                input: Box::new(input),
                failure,
            });
        }
        Ok(Self {
            reference: input.reference,
            kind: input.kind,
            session: input.session,
            accepted_input: input.accepted_input,
            state: input.state,
        })
    }

    /// Applies one permitted transition without changing the pass turn.
    pub fn transition(mut self, next: ReviewPassState) -> Result<Self, ReviewPassTransitionError> {
        let same_turn = match (self.state.turn(), next.turn()) {
            (Some(current), Some(next)) => current == next,
            _ => true,
        };
        let permitted = match (self.state, next) {
            (ReviewPassState::Queued, ReviewPassState::Running { .. }) => true,
            (ReviewPassState::Queued, ReviewPassState::Cancelled { turn: None }) => true,
            (
                ReviewPassState::Running { .. },
                ReviewPassState::Succeeded { .. }
                | ReviewPassState::Failed { .. }
                | ReviewPassState::Blocked { .. }
                | ReviewPassState::Cancelled { turn: Some(_) },
            ) => same_turn,
            _ => false,
        };
        if !permitted {
            return Err(ReviewPassTransitionError {
                current: self.state,
                next,
                failure: if same_turn {
                    ReviewPassTransitionFailure::InvalidTransition
                } else {
                    ReviewPassTransitionFailure::TurnChanged
                },
            });
        }
        self.state = next;
        Ok(self)
    }

    /// Returns the complete pass reference.
    pub const fn reference(&self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the pass kind.
    pub const fn kind(&self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the exact execution session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exact orchestration input.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the current pass state.
    pub const fn state(&self) -> ReviewPassState {
        self.state
    }
}

fn validate_pass_reconstitution(
    input: ReviewPassReconstitutionInput,
) -> Option<ReviewPassReconstitutionFailure> {
    if input.session != input.accepted_input_session {
        return Some(ReviewPassReconstitutionFailure::AcceptedInputSessionMismatch);
    }
    let Some(turn) = input.state.turn() else {
        return input
            .turn_evidence
            .is_some()
            .then_some(ReviewPassReconstitutionFailure::UnexpectedTurnEvidence);
    };
    let Some(evidence) = input.turn_evidence else {
        return Some(ReviewPassReconstitutionFailure::MissingTurnEvidence);
    };
    if evidence.turn != turn {
        return Some(ReviewPassReconstitutionFailure::TurnMismatch);
    }
    if evidence.session != input.session {
        return Some(ReviewPassReconstitutionFailure::TurnSessionMismatch);
    }
    if evidence.accepted_input != input.accepted_input {
        return Some(ReviewPassReconstitutionFailure::TurnAcceptedInputMismatch);
    }
    if let ReviewPassState::Succeeded {
        output_frontier, ..
    } = input.state
        && evidence.terminal_frontier != Some(output_frontier)
    {
        return Some(ReviewPassReconstitutionFailure::OutputFrontierMismatch);
    }
    None
}

/// Rejected queued-pass construction retaining both session facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPassConstructionError {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    session: SessionId,
    accepted_input: AcceptedInputId,
    accepted_input_session: SessionId,
}

impl ReviewPassConstructionError {
    /// Returns the rejected pass reference.
    pub const fn reference(self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the rejected pass kind.
    pub const fn kind(self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the pass and canonical accepted-input sessions.
    pub const fn sessions(self) -> (SessionId, SessionId) {
        (self.session, self.accepted_input_session)
    }

    /// Returns the accepted input whose association did not match.
    pub const fn accepted_input(self) -> AcceptedInputId {
        self.accepted_input
    }
}

/// Why stored pass facts cannot reconstruct one canonical pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassReconstitutionFailure {
    /// The accepted input belongs to another session.
    AcceptedInputSessionMismatch,
    /// The pass names a turn but no canonical turn row was supplied.
    MissingTurnEvidence,
    /// A turn row was supplied for a pass state with no turn.
    UnexpectedTurnEvidence,
    /// The canonical turn identity differs from the pass state.
    TurnMismatch,
    /// The canonical turn belongs to another session.
    TurnSessionMismatch,
    /// The canonical turn was originated by another accepted input.
    TurnAcceptedInputMismatch,
    /// The successful output is not the canonical terminal frontier.
    OutputFrontierMismatch,
}

/// Rejected pass reconstitution retaining every independently stored fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassReconstitutionError {
    input: Box<ReviewPassReconstitutionInput>,
    failure: ReviewPassReconstitutionFailure,
}

impl ReviewPassReconstitutionError {
    /// Returns why the stored facts were rejected.
    pub const fn failure(&self) -> ReviewPassReconstitutionFailure {
        self.failure
    }

    /// Borrows the unchanged stored facts.
    pub const fn input(&self) -> &ReviewPassReconstitutionInput {
        &self.input
    }

    /// Returns the unchanged stored facts.
    pub fn into_input(self) -> ReviewPassReconstitutionInput {
        *self.input
    }
}

/// Why a review-pass transition was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassTransitionFailure {
    /// The requested lifecycle edge is not permitted.
    InvalidTransition,
    /// The transition names a different turn.
    TurnChanged,
}

/// Rejected pass transition retaining both states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPassTransitionError {
    current: ReviewPassState,
    next: ReviewPassState,
    failure: ReviewPassTransitionFailure,
}

impl ReviewPassTransitionError {
    /// Returns why the transition was rejected.
    pub const fn failure(self) -> ReviewPassTransitionFailure {
        self.failure
    }

    /// Returns the current and requested states.
    pub const fn states(self) -> (ReviewPassState, ReviewPassState) {
        (self.current, self.next)
    }
}

/// Side of a diff to which a line range applies.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewFindingDiffSide {
    /// The base or removed side.
    Left,
    /// The head or added side.
    Right,
}

/// A closed positive line range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReviewLineRange {
    start: NonZeroU32,
    end: NonZeroU32,
}

impl ReviewLineRange {
    /// Checks positive ordered endpoints.
    pub const fn try_new(start: u32, end: u32) -> Result<Self, ReviewLineRangeError> {
        let Some(start) = NonZeroU32::new(start) else {
            return Err(ReviewLineRangeError::ZeroEndpoint);
        };
        let Some(end) = NonZeroU32::new(end) else {
            return Err(ReviewLineRangeError::ZeroEndpoint);
        };
        if end.get() < start.get() {
            return Err(ReviewLineRangeError::EndBeforeStart);
        }
        Ok(Self { start, end })
    }

    /// Returns the first line.
    pub const fn start(self) -> u32 {
        self.start.get()
    }

    /// Returns the final line.
    pub const fn end(self) -> u32 {
        self.end.get()
    }
}

/// Why a finding line range is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewLineRangeError {
    /// At least one endpoint is zero.
    ZeroEndpoint,
    /// The end precedes the start.
    EndBeforeStart,
}

/// Exact finding location.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReviewFindingLocation {
    file_path: ReviewKey,
    line_range: Option<ReviewLineRange>,
    diff_side: ReviewFindingDiffSide,
}

impl ReviewFindingLocation {
    /// Constructs an exact path and optional line range.
    pub const fn new(
        file_path: ReviewKey,
        line_range: Option<ReviewLineRange>,
        diff_side: ReviewFindingDiffSide,
    ) -> Self {
        Self {
            file_path,
            line_range,
            diff_side,
        }
    }

    /// Borrows the exact path.
    pub const fn file_path(&self) -> &ReviewKey {
        &self.file_path
    }

    /// Returns the optional closed range.
    pub const fn line_range(&self) -> Option<ReviewLineRange> {
        self.line_range
    }

    /// Returns the diff side.
    pub const fn diff_side(&self) -> ReviewFindingDiffSide {
        self.diff_side
    }
}

/// Review-finding severity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ReviewFindingSeverity {
    /// Informational observation.
    Info,
    /// Low-severity defect.
    Low,
    /// Medium-severity defect.
    Medium,
    /// High-severity defect.
    High,
    /// Critical defect.
    Critical,
}

/// Immutable finding content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFindingContent {
    location: ReviewFindingLocation,
    title: ReviewText,
    body: ReviewText,
    severity: ReviewFindingSeverity,
    confidence: ReviewConfidence,
    category: ReviewKey,
    recommended_fix: Option<ReviewText>,
}

impl ReviewFindingContent {
    /// Constructs checked immutable content.
    pub const fn new(
        location: ReviewFindingLocation,
        title: ReviewText,
        body: ReviewText,
        severity: ReviewFindingSeverity,
        confidence: ReviewConfidence,
        category: ReviewKey,
        recommended_fix: Option<ReviewText>,
    ) -> Self {
        Self {
            location,
            title,
            body,
            severity,
            confidence,
            category,
            recommended_fix,
        }
    }

    /// Borrows the exact location.
    pub const fn location(&self) -> &ReviewFindingLocation {
        &self.location
    }

    /// Borrows the title.
    pub const fn title(&self) -> &ReviewText {
        &self.title
    }

    /// Borrows the body.
    pub const fn body(&self) -> &ReviewText {
        &self.body
    }

    /// Returns severity.
    pub const fn severity(&self) -> ReviewFindingSeverity {
        self.severity
    }

    /// Returns producer confidence.
    pub const fn confidence(&self) -> ReviewConfidence {
        self.confidence
    }

    /// Borrows the category.
    pub const fn category(&self) -> &ReviewKey {
        &self.category
    }

    /// Borrows the optional recommended fix.
    pub const fn recommended_fix(&self) -> Option<&ReviewText> {
        self.recommended_fix.as_ref()
    }
}

/// Immutable proposed finding plus its producing evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFindingProposal {
    reference: ReviewFindingRef,
    producing_pass: ReviewPassRef,
    content: ReviewFindingContent,
}

impl ReviewFindingProposal {
    /// Constructs a proposal produced by a pass in the same exact run.
    pub fn try_new(
        reference: ReviewFindingRef,
        producing_pass: ReviewPassRef,
        content: ReviewFindingContent,
    ) -> Result<Self, ReviewFindingTransitionError> {
        if producing_pass.run() != reference.run() {
            return Err(ReviewFindingTransitionError::proposal(
                ReviewFindingTransitionFailure::ForeignProducingPass,
            ));
        }
        Ok(Self {
            reference,
            producing_pass,
            content,
        })
    }

    /// Returns the complete finding reference.
    pub const fn reference(&self) -> ReviewFindingRef {
        self.reference
    }

    /// Returns the producing pass.
    pub const fn producing_pass(&self) -> ReviewPassRef {
        self.producing_pass
    }

    /// Borrows immutable finding content.
    pub const fn content(&self) -> &ReviewFindingContent {
        &self.content
    }
}

/// A finding-associated external-link reference.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReviewFindingExternalLinkRef {
    finding: ReviewFindingRef,
    link: ReviewExternalLinkId,
}

impl ReviewFindingExternalLinkRef {
    /// Derives a finding-bound reference from one attached canonical link.
    pub fn try_new(
        finding: ReviewFindingRef,
        link: &ReviewExternalLink,
    ) -> Result<Self, ReviewFindingExternalLinkError> {
        let failure = if link.association() != ReviewExternalLinkAssociation::Finding(finding) {
            Some(ReviewFindingExternalLinkFailure::ForeignAssociation)
        } else if link.attachment().is_none() {
            Some(ReviewFindingExternalLinkFailure::NotAttached)
        } else {
            None
        };
        if let Some(failure) = failure {
            return Err(ReviewFindingExternalLinkError {
                finding,
                link: link.id(),
                association: link.association(),
                failure,
            });
        }
        Ok(Self {
            finding,
            link: link.id(),
        })
    }

    /// Returns the finding reference.
    pub const fn finding(self) -> ReviewFindingRef {
        self.finding
    }

    /// Returns the external-link identity.
    pub const fn link(self) -> ReviewExternalLinkId {
        self.link
    }
}

/// Why a canonical external link cannot support a posted finding event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewFindingExternalLinkFailure {
    /// The link is not canonically associated with the finding.
    ForeignAssociation,
    /// The link remains a pending reservation without an external identity.
    NotAttached,
}

/// Rejected finding-link binding retaining the canonical association evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewFindingExternalLinkError {
    finding: ReviewFindingRef,
    link: ReviewExternalLinkId,
    association: ReviewExternalLinkAssociation,
    failure: ReviewFindingExternalLinkFailure,
}

impl ReviewFindingExternalLinkError {
    /// Returns the finding that required publication evidence.
    pub const fn finding(self) -> ReviewFindingRef {
        self.finding
    }

    /// Returns the canonical link identity.
    pub const fn link(self) -> ReviewExternalLinkId {
        self.link
    }

    /// Returns the canonical link association.
    pub const fn association(self) -> ReviewExternalLinkAssociation {
        self.association
    }

    /// Returns why the canonical link was rejected.
    pub const fn failure(self) -> ReviewFindingExternalLinkFailure {
        self.failure
    }
}

/// One typed finding lifecycle event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewFindingEventKind {
    /// A judge accepted the finding.
    Accepted,
    /// A judge rejected the finding.
    Rejected {
        /// Exact reason.
        reason: ReviewText,
    },
    /// Dedupe classified the finding under a canonical finding.
    Duplicate {
        /// Canonical finding in the proposal's run.
        canonical: ReviewFindingRef,
    },
    /// A later finding superseded this finding.
    Superseded {
        /// Successor finding in the proposal's run.
        successor: ReviewFindingRef,
    },
    /// The finding no longer applies to the target.
    Stale,
    /// The finding was published through the exact external link.
    Posted {
        /// Finding-bound external-link reference.
        link: ReviewFindingExternalLinkRef,
    },
    /// A repair pass fixed the finding.
    Fixed,
    /// A repair or publication pass could not proceed.
    BlockedWithReason {
        /// Exact nonempty reason.
        reason: ReviewText,
    },
}

/// One append-only finding lifecycle record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFindingEvent {
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassRef,
    kind: ReviewFindingEventKind,
}

impl ReviewFindingEvent {
    /// Constructs one typed event.
    pub const fn new(
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassRef,
        kind: ReviewFindingEventKind,
    ) -> Self {
        Self {
            ordinal,
            pass,
            kind,
        }
    }

    /// Returns the contiguous one-based ordinal.
    pub const fn ordinal(&self) -> ReviewEventOrdinal {
        self.ordinal
    }

    /// Returns the producing pass.
    pub const fn pass(&self) -> ReviewPassRef {
        self.pass
    }

    /// Borrows the event kind and evidence.
    pub const fn kind(&self) -> &ReviewFindingEventKind {
        &self.kind
    }
}

/// Current state derived from a finding's complete event history.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewFindingStatus {
    /// Proposed and not yet judged.
    Open,
    /// Accepted by judgment.
    Accepted,
    /// Rejected by judgment.
    Rejected,
    /// Classified as a duplicate.
    Duplicate,
    /// Replaced by a later finding.
    Superseded,
    /// No longer applies to the target.
    Stale,
    /// Published externally.
    Posted,
    /// Fixed by a repair pass.
    Fixed,
    /// A publication or repair pass could not proceed.
    BlockedWithReason,
}

/// One finding aggregate with complete append-only event history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFinding {
    proposal: ReviewFindingProposal,
    events: Vec<ReviewFindingEvent>,
    status: ReviewFindingStatus,
}

impl ReviewFinding {
    /// Constructs an open finding.
    pub const fn new(proposal: ReviewFindingProposal) -> Self {
        Self {
            proposal,
            events: Vec::new(),
            status: ReviewFindingStatus::Open,
        }
    }

    /// Reconstitutes a finding by replaying its complete event history.
    pub fn try_reconstitute(
        proposal: ReviewFindingProposal,
        events: Vec<ReviewFindingEvent>,
    ) -> Result<Self, ReviewFindingTransitionError> {
        let mut finding = Self::new(proposal);
        for event in events {
            finding = finding.apply(event)?;
        }
        Ok(finding)
    }

    /// Applies one next contiguous typed event.
    pub fn apply(
        mut self,
        event: ReviewFindingEvent,
    ) -> Result<Self, ReviewFindingTransitionError> {
        let expected = match self.events.last() {
            Some(previous) => previous.ordinal.checked_successor(),
            None => Some(ReviewEventOrdinal::one()),
        };
        if expected != Some(event.ordinal) {
            return Err(ReviewFindingTransitionError {
                event: Some(Box::new(event)),
                failure: ReviewFindingTransitionFailure::NoncontiguousOrdinal { expected },
            });
        }
        if event.pass.target() != self.proposal.reference.target() {
            return Err(ReviewFindingTransitionError {
                event: Some(Box::new(event)),
                failure: ReviewFindingTransitionFailure::ForeignEventPass,
            });
        }
        validate_finding_reference(&self.proposal, &event)?;
        let Some(next_status) = finding_transition(self.status, &event.kind) else {
            return Err(ReviewFindingTransitionError {
                event: Some(Box::new(event)),
                failure: ReviewFindingTransitionFailure::InvalidTransition {
                    current: self.status,
                },
            });
        };
        self.status = next_status;
        self.events.push(event);
        Ok(self)
    }

    /// Borrows immutable proposal content and provenance.
    pub const fn proposal(&self) -> &ReviewFindingProposal {
        &self.proposal
    }

    /// Borrows the complete ordered event history.
    pub fn events(&self) -> &[ReviewFindingEvent] {
        &self.events
    }

    /// Returns the current derived status.
    pub const fn status(&self) -> ReviewFindingStatus {
        self.status
    }
}

fn validate_finding_reference(
    proposal: &ReviewFindingProposal,
    event: &ReviewFindingEvent,
) -> Result<(), ReviewFindingTransitionError> {
    let referenced = match &event.kind {
        ReviewFindingEventKind::Duplicate { canonical } => Some(*canonical),
        ReviewFindingEventKind::Superseded { successor } => Some(*successor),
        ReviewFindingEventKind::Posted { link } => {
            if link.finding() != proposal.reference {
                return Err(ReviewFindingTransitionError {
                    event: Some(Box::new(event.clone())),
                    failure: ReviewFindingTransitionFailure::ForeignExternalLink,
                });
            }
            None
        }
        _ => None,
    };
    if let Some(referenced) = referenced {
        if referenced.run() != proposal.reference.run() {
            return Err(ReviewFindingTransitionError {
                event: Some(Box::new(event.clone())),
                failure: ReviewFindingTransitionFailure::ForeignReferencedFinding,
            });
        }
        if referenced == proposal.reference {
            return Err(ReviewFindingTransitionError {
                event: Some(Box::new(event.clone())),
                failure: ReviewFindingTransitionFailure::SelfReference,
            });
        }
    }
    Ok(())
}

fn finding_transition(
    current: ReviewFindingStatus,
    event: &ReviewFindingEventKind,
) -> Option<ReviewFindingStatus> {
    use ReviewFindingEventKind as Event;
    use ReviewFindingStatus as Status;

    match (current, event) {
        (Status::Open, Event::Accepted) => Some(Status::Accepted),
        (Status::Open, Event::Rejected { .. }) => Some(Status::Rejected),
        (Status::Open | Status::Accepted, Event::Duplicate { .. }) => Some(Status::Duplicate),
        (
            Status::Open | Status::Accepted | Status::Posted | Status::BlockedWithReason,
            Event::Superseded { .. },
        ) => Some(Status::Superseded),
        (
            Status::Open | Status::Accepted | Status::Posted | Status::BlockedWithReason,
            Event::Stale,
        ) => Some(Status::Stale),
        (Status::Accepted, Event::Posted { .. }) => Some(Status::Posted),
        (Status::Accepted | Status::Posted | Status::BlockedWithReason, Event::Fixed) => {
            Some(Status::Fixed)
        }
        (Status::Accepted | Status::Posted, Event::BlockedWithReason { .. }) => {
            Some(Status::BlockedWithReason)
        }
        _ => None,
    }
}

/// Why a finding proposal, event, or complete history is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewFindingTransitionFailure {
    /// The producing pass does not belong to the finding's exact run.
    ForeignProducingPass,
    /// An event pass belongs to another target.
    ForeignEventPass,
    /// A duplicate or successor finding belongs to another run.
    ForeignReferencedFinding,
    /// A finding names itself as its canonical or successor finding.
    SelfReference,
    /// A publication link belongs to another finding.
    ForeignExternalLink,
    /// Event ordinals are not a contiguous one-based sequence.
    NoncontiguousOrdinal {
        /// Expected next ordinal, or `None` after ordinal exhaustion.
        expected: Option<ReviewEventOrdinal>,
    },
    /// The event is not permitted from the current status.
    InvalidTransition {
        /// Status before the rejected event.
        current: ReviewFindingStatus,
    },
}

/// Rejected finding construction or event application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFindingTransitionError {
    event: Option<Box<ReviewFindingEvent>>,
    failure: ReviewFindingTransitionFailure,
}

impl ReviewFindingTransitionError {
    fn proposal(failure: ReviewFindingTransitionFailure) -> Self {
        Self {
            event: None,
            failure,
        }
    }

    /// Returns why construction or transition failed.
    pub const fn failure(&self) -> ReviewFindingTransitionFailure {
        self.failure
    }

    /// Borrows the rejected event, when failure occurred during event application.
    pub fn event(&self) -> Option<&ReviewFindingEvent> {
        match self.event.as_ref() {
            Some(event) => Some(event.as_ref()),
            None => None,
        }
    }

    /// Returns the rejected event and failure.
    pub fn into_parts(
        self,
    ) -> (
        Option<Box<ReviewFindingEvent>>,
        ReviewFindingTransitionFailure,
    ) {
        (self.event, self.failure)
    }
}

/// Which aggregate an external link belongs to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewExternalLinkAssociation {
    /// The link describes the target itself.
    Target(ReviewTargetId),
    /// The link describes one run.
    Run(ReviewRunRef),
    /// The link describes one finding.
    Finding(ReviewFindingRef),
}

impl ReviewExternalLinkAssociation {
    /// Returns the owning target.
    pub const fn target(self) -> ReviewTargetId {
        match self {
            Self::Target(target) => target,
            Self::Run(run) => run.target(),
            Self::Finding(finding) => finding.target(),
        }
    }
}

/// Closed kind of external review object.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewExternalObjectKind {
    /// External change request.
    ChangeRequest,
    /// External commit.
    Commit,
    /// Whole review.
    Review,
    /// Review thread.
    ReviewThread,
    /// Inline review comment.
    ReviewComment,
    /// General change-request comment.
    ChangeRequestComment,
}

/// One immutable external-object attachment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkAttachment {
    pass: ReviewPassRef,
    external_object: ReviewKey,
}

impl ReviewExternalLinkAttachment {
    /// Constructs attachment evidence.
    pub const fn new(pass: ReviewPassRef, external_object: ReviewKey) -> Self {
        Self {
            pass,
            external_object,
        }
    }

    /// Returns the producing pass.
    pub const fn pass(&self) -> ReviewPassRef {
        self.pass
    }

    /// Borrows the exact external object identity.
    pub const fn external_object(&self) -> &ReviewKey {
        &self.external_object
    }
}

/// Externally reported object state.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewExternalObjectState {
    /// Object applies to the current revision.
    Current,
    /// Object applies only to an older revision.
    Outdated,
    /// External system reports the object resolved.
    Resolved,
}

/// One append-only external-state observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkObservation {
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassRef,
    state: ReviewExternalObjectState,
}

impl ReviewExternalLinkObservation {
    /// Constructs one observation.
    pub const fn new(
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassRef,
        state: ReviewExternalObjectState,
    ) -> Self {
        Self {
            ordinal,
            pass,
            state,
        }
    }

    /// Returns the contiguous one-based ordinal.
    pub const fn ordinal(self) -> ReviewEventOrdinal {
        self.ordinal
    }

    /// Returns the observing pass.
    pub const fn pass(self) -> ReviewPassRef {
        self.pass
    }

    /// Returns the reported state.
    pub const fn state(self) -> ReviewExternalObjectState {
        self.state
    }
}

/// One durable external-link reservation with optional attachment and observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewExternalLink {
    id: ReviewExternalLinkId,
    association: ReviewExternalLinkAssociation,
    provider: ReviewKey,
    object_kind: ReviewExternalObjectKind,
    attachment: Option<ReviewExternalLinkAttachment>,
    observations: Vec<ReviewExternalLinkObservation>,
}

impl ReviewExternalLink {
    /// Constructs the durable pre-effect reservation.
    pub const fn reserve(
        id: ReviewExternalLinkId,
        association: ReviewExternalLinkAssociation,
        provider: ReviewKey,
        object_kind: ReviewExternalObjectKind,
    ) -> Self {
        Self {
            id,
            association,
            provider,
            object_kind,
            attachment: None,
            observations: Vec::new(),
        }
    }

    /// Reconstitutes a complete link by validating attachment and observations.
    pub fn try_reconstitute(
        id: ReviewExternalLinkId,
        association: ReviewExternalLinkAssociation,
        provider: ReviewKey,
        object_kind: ReviewExternalObjectKind,
        attachment: Option<ReviewExternalLinkAttachment>,
        observations: Vec<ReviewExternalLinkObservation>,
    ) -> Result<Self, ReviewExternalLinkTransitionError> {
        let mut link = Self::reserve(id, association, provider, object_kind);
        if let Some(attachment) = attachment {
            link = link.attach(attachment)?;
        }
        for observation in observations {
            link = link.observe(observation)?;
        }
        Ok(link)
    }

    /// Attaches the exact external identity after reservation.
    pub fn attach(
        mut self,
        attachment: ReviewExternalLinkAttachment,
    ) -> Result<Self, ReviewExternalLinkTransitionError> {
        if self.attachment.is_some() {
            return Err(ReviewExternalLinkTransitionError::AlreadyAttached);
        }
        if attachment.pass.target() != self.association.target() {
            return Err(ReviewExternalLinkTransitionError::ForeignPass);
        }
        self.attachment = Some(attachment);
        Ok(self)
    }

    /// Appends one same-target contiguous observation after attachment.
    pub fn observe(
        mut self,
        observation: ReviewExternalLinkObservation,
    ) -> Result<Self, ReviewExternalLinkTransitionError> {
        if self.attachment.is_none() {
            return Err(ReviewExternalLinkTransitionError::NotAttached);
        }
        if observation.pass.target() != self.association.target() {
            return Err(ReviewExternalLinkTransitionError::ForeignPass);
        }
        let expected = match self.observations.last() {
            Some(previous) => previous.ordinal.checked_successor(),
            None => Some(ReviewEventOrdinal::one()),
        };
        if expected != Some(observation.ordinal) {
            return Err(ReviewExternalLinkTransitionError::NoncontiguousOrdinal { expected });
        }
        self.observations.push(observation);
        Ok(self)
    }

    /// Returns the reservation identity and idempotency key.
    pub const fn id(&self) -> ReviewExternalLinkId {
        self.id
    }

    /// Returns the canonical aggregate association.
    pub const fn association(&self) -> ReviewExternalLinkAssociation {
        self.association
    }

    /// Borrows the opaque provider key.
    pub const fn provider(&self) -> &ReviewKey {
        &self.provider
    }

    /// Returns the external object kind.
    pub const fn object_kind(&self) -> ReviewExternalObjectKind {
        self.object_kind
    }

    /// Borrows the optional immutable attachment.
    pub const fn attachment(&self) -> Option<&ReviewExternalLinkAttachment> {
        self.attachment.as_ref()
    }

    /// Borrows all external-state observations.
    pub fn observations(&self) -> &[ReviewExternalLinkObservation] {
        &self.observations
    }
}

/// Why external-link attachment, observation, or reconstitution failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewExternalLinkTransitionError {
    /// A second attachment was attempted.
    AlreadyAttached,
    /// Attachment or observation evidence belongs to another target.
    ForeignPass,
    /// An observation was supplied before attachment.
    NotAttached,
    /// Observation ordinals are not a contiguous one-based sequence.
    NoncontiguousOrdinal {
        /// Expected next ordinal, or `None` after ordinal exhaustion.
        expected: Option<ReviewEventOrdinal>,
    },
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use explicit fixture expectations"
)]
mod tests {
    use expect_test::expect;
    use signalbox_expect_table::table;
    use uuid::Uuid;

    use super::*;

    fn target_id(value: u128) -> ReviewTargetId {
        ReviewTargetId::from_uuid(Uuid::from_u128(value))
    }

    fn run_id(value: u128) -> ReviewRunId {
        ReviewRunId::from_uuid(Uuid::from_u128(value))
    }

    fn pass_id(value: u128) -> ReviewPassId {
        ReviewPassId::from_uuid(Uuid::from_u128(value))
    }

    fn finding_id(value: u128) -> ReviewFindingId {
        ReviewFindingId::from_uuid(Uuid::from_u128(value))
    }

    fn link_id(value: u128) -> ReviewExternalLinkId {
        ReviewExternalLinkId::from_uuid(Uuid::from_u128(value))
    }

    fn session_id(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    fn accepted_input_id(value: u128) -> AcceptedInputId {
        AcceptedInputId::from_uuid(Uuid::from_u128(value))
    }

    fn turn_id(value: u128) -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(value))
    }

    fn frontier_id(value: u128) -> ContextFrontierId {
        ContextFrontierId::from_uuid(Uuid::from_u128(value))
    }

    fn key(value: &str) -> ReviewKey {
        ReviewKey::try_new(String::from(value)).expect("fixture key is valid")
    }

    fn text(value: &str) -> ReviewText {
        ReviewText::try_new(String::from(value)).expect("fixture text is valid")
    }

    fn run_ref() -> ReviewRunRef {
        ReviewRunRef::new(target_id(1), run_id(2))
    }

    fn pass_ref(value: u128) -> ReviewPassRef {
        ReviewPassRef::new(run_ref(), pass_id(value))
    }

    fn finding_ref(value: u128) -> ReviewFindingRef {
        ReviewFindingRef::new(run_ref(), finding_id(value))
    }

    fn proposal() -> ReviewFindingProposal {
        ReviewFindingProposal::try_new(
            finding_ref(10),
            pass_ref(3),
            ReviewFindingContent::new(
                ReviewFindingLocation::new(
                    key("src/lib.rs"),
                    Some(ReviewLineRange::try_new(4, 7).expect("ordered line range")),
                    ReviewFindingDiffSide::Right,
                ),
                text("Finding title"),
                text("Finding body"),
                ReviewFindingSeverity::High,
                ReviewConfidence::try_from_basis_points(8_500).expect("bounded confidence"),
                key("correctness"),
                Some(text("Apply the exact fix")),
            ),
        )
        .expect("producing pass belongs to the finding run")
    }

    fn attached_finding_link(
        finding: ReviewFindingRef,
        link: ReviewExternalLinkId,
    ) -> ReviewExternalLink {
        ReviewExternalLink::reserve(
            link,
            ReviewExternalLinkAssociation::Finding(finding),
            key("code-host"),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(ReviewExternalLinkAttachment::new(
            pass_ref(20),
            key("external-comment-42"),
        ))
        .expect("fixture attachment belongs to the finding target")
    }

    fn finding_link_ref(
        finding: ReviewFindingRef,
        link: ReviewExternalLinkId,
    ) -> ReviewFindingExternalLinkRef {
        ReviewFindingExternalLinkRef::try_new(finding, &attached_finding_link(finding, link))
            .expect("fixture link is attached to the exact finding")
    }

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct FindingTransitionRow {
        current: String,
        permitted_events: String,
    }

    /// INV-040: complete references reject cross-wired run and pass state.
    #[test]
    fn inv040_run_and_pass_transitions_preserve_exact_evidence() {
        let reference = run_ref();
        let active = pass_ref(3);
        let foreign = ReviewPassRef::new(ReviewRunRef::new(target_id(1), run_id(99)), pass_id(3));
        let queued = ReviewRun::new(
            reference,
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let requested = ReviewRunState::Running {
            active_pass: foreign,
        };
        let cross_wired = queued
            .transition(requested)
            .expect_err("foreign pass must fail closed");
        assert_eq!(
            cross_wired.failure(),
            ReviewRunTransitionFailure::ForeignPass
        );
        assert_eq!(cross_wired.states(), (ReviewRunState::Queued, requested));

        let cross_wired_input = ReviewPass::try_new(
            active,
            ReviewPassKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            session_id(99),
        )
        .expect_err("accepted input must belong to the pass session");
        assert_eq!(
            cross_wired_input.sessions(),
            (session_id(4), session_id(99))
        );

        let running = ReviewPass::try_new(
            active,
            ReviewPassKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            session_id(4),
        )
        .expect("accepted input belongs to the pass session")
        .transition(ReviewPassState::Running { turn: turn_id(6) })
        .expect("queued pass may activate");
        let changed_turn = running
            .transition(ReviewPassState::Succeeded {
                turn: turn_id(7),
                output_frontier: frontier_id(8),
            })
            .expect_err("terminal evidence must retain the active turn");
        assert_eq!(
            changed_turn.failure(),
            ReviewPassTransitionFailure::TurnChanged
        );
    }

    /// INV-040: reconstitution checks accepted-input, turn, and frontier
    /// evidence loaded independently from the pass row.
    #[test]
    fn inv040_pass_reconstitution_rejects_cross_wired_canonical_evidence() {
        let state = ReviewPassState::Succeeded {
            turn: turn_id(6),
            output_frontier: frontier_id(8),
        };
        let exact_turn = ReviewPassTurnEvidence::new(
            turn_id(6),
            session_id(4),
            accepted_input_id(5),
            Some(frontier_id(8)),
        );
        let exact = ReviewPassReconstitutionInput::new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            session_id(4),
            state,
            Some(exact_turn),
        );
        assert_eq!(
            ReviewPass::try_reconstitute(exact)
                .expect("all canonical evidence matches")
                .state(),
            state
        );

        let cases = [
            (
                ReviewPassReconstitutionInput::new(
                    pass_ref(3),
                    ReviewPassKind::ReadOnlyReview,
                    session_id(4),
                    accepted_input_id(5),
                    session_id(9),
                    state,
                    Some(exact_turn),
                ),
                ReviewPassReconstitutionFailure::AcceptedInputSessionMismatch,
            ),
            (
                ReviewPassReconstitutionInput::new(
                    pass_ref(3),
                    ReviewPassKind::ReadOnlyReview,
                    session_id(4),
                    accepted_input_id(5),
                    session_id(4),
                    state,
                    None,
                ),
                ReviewPassReconstitutionFailure::MissingTurnEvidence,
            ),
            (
                ReviewPassReconstitutionInput::new(
                    pass_ref(3),
                    ReviewPassKind::ReadOnlyReview,
                    session_id(4),
                    accepted_input_id(5),
                    session_id(4),
                    state,
                    Some(ReviewPassTurnEvidence::new(
                        turn_id(7),
                        session_id(4),
                        accepted_input_id(5),
                        Some(frontier_id(8)),
                    )),
                ),
                ReviewPassReconstitutionFailure::TurnMismatch,
            ),
            (
                ReviewPassReconstitutionInput::new(
                    pass_ref(3),
                    ReviewPassKind::ReadOnlyReview,
                    session_id(4),
                    accepted_input_id(5),
                    session_id(4),
                    state,
                    Some(ReviewPassTurnEvidence::new(
                        turn_id(6),
                        session_id(9),
                        accepted_input_id(5),
                        Some(frontier_id(8)),
                    )),
                ),
                ReviewPassReconstitutionFailure::TurnSessionMismatch,
            ),
            (
                ReviewPassReconstitutionInput::new(
                    pass_ref(3),
                    ReviewPassKind::ReadOnlyReview,
                    session_id(4),
                    accepted_input_id(5),
                    session_id(4),
                    state,
                    Some(ReviewPassTurnEvidence::new(
                        turn_id(6),
                        session_id(4),
                        accepted_input_id(9),
                        Some(frontier_id(8)),
                    )),
                ),
                ReviewPassReconstitutionFailure::TurnAcceptedInputMismatch,
            ),
            (
                ReviewPassReconstitutionInput::new(
                    pass_ref(3),
                    ReviewPassKind::ReadOnlyReview,
                    session_id(4),
                    accepted_input_id(5),
                    session_id(4),
                    state,
                    Some(ReviewPassTurnEvidence::new(
                        turn_id(6),
                        session_id(4),
                        accepted_input_id(5),
                        Some(frontier_id(9)),
                    )),
                ),
                ReviewPassReconstitutionFailure::OutputFrontierMismatch,
            ),
            (
                ReviewPassReconstitutionInput::new(
                    pass_ref(3),
                    ReviewPassKind::ReadOnlyReview,
                    session_id(4),
                    accepted_input_id(5),
                    session_id(4),
                    ReviewPassState::Queued,
                    Some(exact_turn),
                ),
                ReviewPassReconstitutionFailure::UnexpectedTurnEvidence,
            ),
        ];
        for (input, expected) in cases {
            let error =
                ReviewPass::try_reconstitute(input).expect_err("cross-wiring must fail closed");
            assert_eq!(error.failure(), expected);
            assert_eq!(*error.input(), input);
        }
    }

    /// INV-040 / INV-041: a posted finding consumes an attached canonical link
    /// associated with that exact finding.
    #[test]
    fn inv040_posted_link_rejects_pending_and_foreign_canonical_associations() {
        let finding = finding_ref(10);
        let pending = ReviewExternalLink::reserve(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding),
            key("code-host"),
            ReviewExternalObjectKind::ReviewComment,
        );
        let pending_error = ReviewFindingExternalLinkRef::try_new(finding, &pending)
            .expect_err("pending reservation is not posting evidence");
        assert_eq!(
            pending_error.failure(),
            ReviewFindingExternalLinkFailure::NotAttached
        );

        let foreign = attached_finding_link(finding_ref(11), link_id(31));
        let foreign_error = ReviewFindingExternalLinkRef::try_new(finding, &foreign)
            .expect_err("canonical association belongs to another finding");
        assert_eq!(
            foreign_error.failure(),
            ReviewFindingExternalLinkFailure::ForeignAssociation
        );
        assert_eq!(
            foreign_error.association(),
            ReviewExternalLinkAssociation::Finding(finding_ref(11))
        );

        let exact = attached_finding_link(finding, link_id(32));
        assert_eq!(
            ReviewFindingExternalLinkRef::try_new(finding, &exact)
                .expect("attached canonical association supports posting")
                .link(),
            link_id(32)
        );
    }

    /// INV-040: finding state is derived from one contiguous typed history.
    #[test]
    fn inv040_finding_machine_rejects_gaps_and_terminal_reopening() {
        let finding = ReviewFinding::new(proposal())
            .apply(ReviewFindingEvent::new(
                ReviewEventOrdinal::one(),
                pass_ref(20),
                ReviewFindingEventKind::Accepted,
            ))
            .expect("open finding may be accepted")
            .apply(ReviewFindingEvent::new(
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                pass_ref(21),
                ReviewFindingEventKind::Posted {
                    link: finding_link_ref(finding_ref(10), link_id(30)),
                },
            ))
            .expect("accepted finding may be posted")
            .apply(ReviewFindingEvent::new(
                ReviewEventOrdinal::try_new(3).expect("positive ordinal"),
                pass_ref(22),
                ReviewFindingEventKind::Fixed,
            ))
            .expect("posted finding may be fixed");
        assert_eq!(finding.status(), ReviewFindingStatus::Fixed);

        let reopened = finding
            .apply(ReviewFindingEvent::new(
                ReviewEventOrdinal::try_new(4).expect("positive ordinal"),
                pass_ref(23),
                ReviewFindingEventKind::Accepted,
            ))
            .expect_err("fixed finding is terminal");
        assert_eq!(
            reopened.failure(),
            ReviewFindingTransitionFailure::InvalidTransition {
                current: ReviewFindingStatus::Fixed
            }
        );

        let gap = ReviewFinding::new(proposal())
            .apply(ReviewFindingEvent::new(
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                pass_ref(20),
                ReviewFindingEventKind::Accepted,
            ))
            .expect_err("history must begin at ordinal one");
        assert_eq!(
            gap.failure(),
            ReviewFindingTransitionFailure::NoncontiguousOrdinal {
                expected: Some(ReviewEventOrdinal::one())
            }
        );
    }

    /// INV-040: the complete nine-state finding transition surface stays
    /// closed and reviewable as one table.
    #[test]
    fn inv040_finding_transition_matrix_is_closed() {
        let statuses = [
            ReviewFindingStatus::Open,
            ReviewFindingStatus::Accepted,
            ReviewFindingStatus::Rejected,
            ReviewFindingStatus::Duplicate,
            ReviewFindingStatus::Superseded,
            ReviewFindingStatus::Stale,
            ReviewFindingStatus::Posted,
            ReviewFindingStatus::Fixed,
            ReviewFindingStatus::BlockedWithReason,
        ];
        let events = [
            (
                "Accepted",
                ReviewFindingEventKind::Accepted,
                ReviewFindingStatus::Accepted,
            ),
            (
                "Rejected",
                ReviewFindingEventKind::Rejected {
                    reason: text("rejected"),
                },
                ReviewFindingStatus::Rejected,
            ),
            (
                "Duplicate",
                ReviewFindingEventKind::Duplicate {
                    canonical: finding_ref(11),
                },
                ReviewFindingStatus::Duplicate,
            ),
            (
                "Superseded",
                ReviewFindingEventKind::Superseded {
                    successor: finding_ref(12),
                },
                ReviewFindingStatus::Superseded,
            ),
            (
                "Stale",
                ReviewFindingEventKind::Stale,
                ReviewFindingStatus::Stale,
            ),
            (
                "Posted",
                ReviewFindingEventKind::Posted {
                    link: finding_link_ref(finding_ref(10), link_id(30)),
                },
                ReviewFindingStatus::Posted,
            ),
            (
                "Fixed",
                ReviewFindingEventKind::Fixed,
                ReviewFindingStatus::Fixed,
            ),
            (
                "BlockedWithReason",
                ReviewFindingEventKind::BlockedWithReason {
                    reason: text("blocked"),
                },
                ReviewFindingStatus::BlockedWithReason,
            ),
        ];
        let expected_admission = [
            [true, true, true, true, true, false, false, false],
            [false, false, true, true, true, true, true, true],
            [false; 8],
            [false; 8],
            [false; 8],
            [false; 8],
            [false, false, false, true, true, false, true, true],
            [false; 8],
            [false, false, false, true, true, false, true, false],
        ];
        let rows = statuses
            .into_iter()
            .enumerate()
            .map(|(status_index, current)| {
                let permitted_events = events
                    .iter()
                    .enumerate()
                    .filter_map(|(event_index, (name, event, next))| {
                        let actual = finding_transition(current, event);
                        let expected =
                            expected_admission[status_index][event_index].then_some(*next);
                        assert_eq!(
                            actual, expected,
                            "finding edge {current:?} through {name} must match the closed machine"
                        );
                        actual.map(|_| *name)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                FindingTransitionRow {
                    current: format!("{current:?}"),
                    permitted_events: if permitted_events.is_empty() {
                        String::from("-")
                    } else {
                        permitted_events
                    },
                }
            })
            .collect::<Vec<_>>();

        expect![[r#"
            ┌───────────────────┬────────────────────────────────────────────────────────────────┐
            │ current           │ permitted_events                                               │
            ├───────────────────┼────────────────────────────────────────────────────────────────┤
            │ Open              │ Accepted, Rejected, Duplicate, Superseded, Stale               │
            │ Accepted          │ Duplicate, Superseded, Stale, Posted, Fixed, BlockedWithReason │
            │ Rejected          │ -                                                              │
            │ Duplicate         │ -                                                              │
            │ Superseded        │ -                                                              │
            │ Stale             │ -                                                              │
            │ Posted            │ Superseded, Stale, Fixed, BlockedWithReason                    │
            │ Fixed             │ -                                                              │
            │ BlockedWithReason │ Superseded, Stale, Fixed                                       │
            └───────────────────┴────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }

    /// INV-041: attachment follows reservation and all evidence stays on-target.
    #[test]
    fn inv041_external_link_requires_reservation_before_observation() {
        let association = ReviewExternalLinkAssociation::Finding(finding_ref(10));
        let pending = ReviewExternalLink::reserve(
            link_id(30),
            association,
            key("code-host"),
            ReviewExternalObjectKind::ReviewComment,
        );
        assert!(pending.attachment().is_none());

        let premature = pending
            .clone()
            .observe(ReviewExternalLinkObservation::new(
                ReviewEventOrdinal::one(),
                pass_ref(20),
                ReviewExternalObjectState::Current,
            ))
            .expect_err("observation cannot prove an unattached effect");
        assert_eq!(premature, ReviewExternalLinkTransitionError::NotAttached);

        let attached = pending
            .attach(ReviewExternalLinkAttachment::new(
                pass_ref(20),
                key("external-comment-42"),
            ))
            .expect("same-target pass may attach the reservation")
            .observe(ReviewExternalLinkObservation::new(
                ReviewEventOrdinal::one(),
                pass_ref(21),
                ReviewExternalObjectState::Current,
            ))
            .expect("attached link admits contiguous observations");
        assert_eq!(
            attached.attachment().map(|value| value.external_object()),
            Some(&key("external-comment-42"))
        );
    }

    #[test]
    fn policy_and_exact_text_bounds_are_closed() {
        assert_eq!(
            ReviewPolicy::version_one()
                .minimum_judge_confidence()
                .basis_points(),
            7_000
        );
        let invalid = ReviewPolicy::try_new(
            ReviewPolicyVersion::one(),
            ReviewConfidence::try_from_basis_points(8_001).expect("bounded confidence"),
            ReviewConfidence::try_from_basis_points(8_000).expect("bounded confidence"),
        )
        .expect_err("publication cannot be easier than judgment");
        assert_eq!(
            invalid.into_parts().1.basis_points(),
            8_001,
            "rejected policy remains inspectable"
        );
        let noncanonical_version_one = ReviewPolicy::try_new(
            ReviewPolicyVersion::one(),
            ReviewConfidence::try_from_basis_points(7_000).expect("bounded confidence"),
            ReviewConfidence::try_from_basis_points(8_001).expect("bounded confidence"),
        )
        .expect_err("version one has one exact threshold tuple");
        assert_eq!(
            noncanonical_version_one.into_parts(),
            (
                ReviewPolicyVersion::one(),
                ReviewConfidence::try_from_basis_points(7_000).expect("bounded confidence"),
                ReviewConfidence::try_from_basis_points(8_001).expect("bounded confidence"),
            )
        );
        ReviewPolicy::try_new(
            ReviewPolicyVersion::try_new(2).expect("positive version"),
            ReviewConfidence::try_from_basis_points(7_000).expect("bounded confidence"),
            ReviewConfidence::try_from_basis_points(8_001).expect("bounded confidence"),
        )
        .expect("later versions admit their own ordered threshold tuples");

        let too_long = ReviewKey::try_new("a".repeat(REVIEW_KEY_MAXIMUM_BYTES + 1))
            .expect_err("keys are bounded");
        assert_eq!(
            too_long.failure(),
            ReviewValueFailure::TooLong {
                maximum_bytes: REVIEW_KEY_MAXIMUM_BYTES
            }
        );
    }
}
