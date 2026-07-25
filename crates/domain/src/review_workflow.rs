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

/// Canonical topology facts for one review target's immediate stack parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewTargetParentRef {
    target: ReviewTargetId,
    provider: ReviewKey,
    repository: ReviewKey,
}

impl ReviewTargetParentRef {
    /// Supplies the parent's canonical identity and repository scope.
    pub const fn new(target: ReviewTargetId, provider: ReviewKey, repository: ReviewKey) -> Self {
        Self {
            target,
            provider,
            repository,
        }
    }

    /// Returns the parent target identity.
    pub const fn target(&self) -> ReviewTargetId {
        self.target
    }

    /// Borrows the parent's canonical provider key.
    pub const fn provider(&self) -> &ReviewKey {
        &self.provider
    }

    /// Borrows the parent's canonical repository key.
    pub const fn repository(&self) -> &ReviewKey {
        &self.repository
    }
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
    stack_parent: Option<ReviewTargetParentRef>,
}

impl ReviewTarget {
    /// Constructs a target snapshot, rejecting incomplete comparison evidence
    /// or a self-parent edge.
    pub fn try_new(
        id: ReviewTargetId,
        provider: ReviewKey,
        repository: ReviewKey,
        subject: ReviewTargetSubject,
        head_revision: ReviewKey,
        base_revision: Option<ReviewKey>,
        stack_parent: Option<ReviewTargetParentRef>,
    ) -> Result<Self, ReviewTargetError> {
        if matches!(subject, ReviewTargetSubject::ChangeRequest(_)) && base_revision.is_none() {
            return Err(ReviewTargetError::MissingChangeRequestBase { target: id });
        }
        if let Some(parent) = &stack_parent {
            if parent.target == id {
                return Err(ReviewTargetError::SelfParent { target: id });
            }
            if parent.provider != provider || parent.repository != repository {
                return Err(ReviewTargetError::ForeignParent { target: id });
            }
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
    pub const fn stack_parent(&self) -> Option<&ReviewTargetParentRef> {
        self.stack_parent.as_ref()
    }
}

/// Invalid immutable target snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewTargetError {
    /// A moving change request lacks its frozen comparison revision.
    MissingChangeRequestBase {
        /// The incomplete target.
        target: ReviewTargetId,
    },
    /// The target names itself as its stack parent.
    SelfParent {
        /// The self-parented target.
        target: ReviewTargetId,
    },
    /// The parent belongs to another provider or repository.
    ForeignParent {
        /// The child target whose topology was rejected.
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

/// A finding reference carrying complete target/run/producing-pass ancestry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReviewFindingRef {
    pass: ReviewPassRef,
    finding: ReviewFindingId,
}

impl ReviewFindingRef {
    /// Binds one finding identity to its exact producing pass.
    pub const fn new(pass: ReviewPassRef, finding: ReviewFindingId) -> Self {
        Self { pass, finding }
    }

    /// Returns the producing-pass reference.
    pub const fn pass(self) -> ReviewPassRef {
        self.pass
    }

    /// Returns the run reference.
    pub const fn run(self) -> ReviewRunRef {
        self.pass.run()
    }

    /// Returns the finding identity.
    pub const fn finding(self) -> ReviewFindingId {
        self.finding
    }

    /// Returns the target identity.
    pub const fn target(self) -> ReviewTargetId {
        self.pass.target()
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

/// Canonical pass facts independently loaded for review claims and projections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPassEvidence {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    state: ReviewPassState,
}

impl ReviewPassEvidence {
    /// Supplies one independently stored pass reference, kind, and current state.
    pub const fn new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        state: ReviewPassState,
    ) -> Self {
        Self {
            reference,
            kind,
            state,
        }
    }

    /// Returns the canonical pass reference.
    pub const fn reference(self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the canonical pass kind.
    pub const fn kind(self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the canonical pass state.
    pub const fn state(self) -> ReviewPassState {
        self.state
    }
}

/// Complete independently stored facts for review-run reconstitution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewRunReconstitutionInput {
    reference: ReviewRunRef,
    workflow: ReviewWorkflowKind,
    policy: ReviewPolicy,
    state: ReviewRunState,
    pass_evidence: Option<ReviewPassEvidence>,
}

impl ReviewRunReconstitutionInput {
    /// Supplies the run row and canonical referenced-pass projection.
    pub const fn new(
        reference: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        policy: ReviewPolicy,
        state: ReviewRunState,
        pass_evidence: Option<ReviewPassEvidence>,
    ) -> Self {
        Self {
            reference,
            workflow,
            policy,
            state,
            pass_evidence,
        }
    }

    /// Returns the complete run reference.
    pub const fn reference(self) -> ReviewRunRef {
        self.reference
    }

    /// Returns the workflow kind.
    pub const fn workflow(self) -> ReviewWorkflowKind {
        self.workflow
    }

    /// Returns the frozen policy.
    pub const fn policy(self) -> ReviewPolicy {
        self.policy
    }

    /// Returns the stored run state.
    pub const fn state(self) -> ReviewRunState {
        self.state
    }

    /// Returns canonical pass evidence, when the run names a pass.
    pub const fn pass_evidence(self) -> Option<ReviewPassEvidence> {
        self.pass_evidence
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

    /// Reconstitutes a run after validating its canonical pass evidence.
    pub fn try_reconstitute(
        input: ReviewRunReconstitutionInput,
    ) -> Result<Self, ReviewRunReconstitutionError> {
        validate_run_state(
            input.reference,
            input.workflow,
            input.state,
            input.pass_evidence,
        )
        .map_err(|failure| ReviewRunReconstitutionError {
            input: Box::new(input),
            failure,
        })?;
        Ok(Self {
            reference: input.reference,
            workflow: input.workflow,
            policy: input.policy,
            state: input.state,
        })
    }

    /// Applies one permitted state transition authenticated by canonical pass
    /// evidence.
    pub fn transition(
        mut self,
        next: ReviewRunState,
        pass_evidence: Option<ReviewPassEvidence>,
    ) -> Result<Self, ReviewRunTransitionError> {
        validate_run_state(self.reference, self.workflow, next, pass_evidence).map_err(
            |failure| ReviewRunTransitionError {
                attempt: Box::new(ReviewRunTransitionAttempt {
                    current: self.state,
                    next,
                    pass_evidence,
                }),
                failure: ReviewRunTransitionFailure::Evidence(failure),
            },
        )?;
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
                attempt: Box::new(ReviewRunTransitionAttempt {
                    current: self.state,
                    next,
                    pass_evidence,
                }),
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
    workflow: ReviewWorkflowKind,
    state: ReviewRunState,
    pass_evidence: Option<ReviewPassEvidence>,
) -> Result<(), ReviewRunEvidenceFailure> {
    if let Some(pass) = state.pass()
        && pass.run() != reference
    {
        return Err(ReviewRunEvidenceFailure::ForeignPass);
    }
    let Some(pass) = state.pass() else {
        return if pass_evidence.is_some() {
            Err(ReviewRunEvidenceFailure::UnexpectedPassEvidence)
        } else {
            Ok(())
        };
    };
    let Some(evidence) = pass_evidence else {
        return Err(ReviewRunEvidenceFailure::MissingPassEvidence);
    };
    if evidence.reference != pass {
        return Err(ReviewRunEvidenceFailure::PassMismatch);
    }
    if !workflow_matches_pass_kind(workflow, evidence.kind) {
        return Err(ReviewRunEvidenceFailure::PassKindMismatch);
    }
    if !run_state_matches_pass(state, evidence.state) {
        return Err(ReviewRunEvidenceFailure::PassStateMismatch);
    }
    Ok(())
}

fn workflow_matches_pass_kind(workflow: ReviewWorkflowKind, pass: ReviewPassKind) -> bool {
    matches!(
        (workflow, pass),
        (
            ReviewWorkflowKind::ImportExternalContext,
            ReviewPassKind::ImportExternalContext
        ) | (
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPassKind::ReadOnlyReview
        ) | (ReviewWorkflowKind::JudgeFindings, ReviewPassKind::Judge)
            | (ReviewWorkflowKind::DedupeFindings, ReviewPassKind::Dedupe)
            | (ReviewWorkflowKind::PublishReview, ReviewPassKind::Publish)
            | (ReviewWorkflowKind::FixFindings, ReviewPassKind::Fix)
            | (
                ReviewWorkflowKind::PropagateStack,
                ReviewPassKind::PropagateStack
            )
    )
}

fn run_state_matches_pass(run: ReviewRunState, pass: ReviewPassState) -> bool {
    matches!(
        (run, pass),
        (
            ReviewRunState::Running { .. },
            ReviewPassState::Running { .. }
        ) | (
            ReviewRunState::Succeeded { .. },
            ReviewPassState::Succeeded { .. }
        ) | (
            ReviewRunState::Failed { .. },
            ReviewPassState::Failed { .. }
        ) | (
            ReviewRunState::Blocked { .. },
            ReviewPassState::Blocked { .. }
        ) | (
            ReviewRunState::Cancelled { last_pass: Some(_) },
            ReviewPassState::Cancelled { turn: Some(_) }
        )
    )
}

/// Why canonical pass facts cannot support a run projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewRunEvidenceFailure {
    /// The run state's pass belongs to another run.
    ForeignPass,
    /// The run names a pass but no canonical pass row was supplied.
    MissingPassEvidence,
    /// A canonical pass row was supplied for a run state with no pass.
    UnexpectedPassEvidence,
    /// The canonical pass identity differs from the run projection.
    PassMismatch,
    /// The canonical pass kind is incompatible with the run workflow.
    PassKindMismatch,
    /// The canonical pass outcome differs from the run projection.
    PassStateMismatch,
}

/// Rejected run reconstitution retaining every independently stored fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRunReconstitutionError {
    input: Box<ReviewRunReconstitutionInput>,
    failure: ReviewRunEvidenceFailure,
}

impl ReviewRunReconstitutionError {
    /// Returns why the stored facts were rejected.
    pub const fn failure(&self) -> ReviewRunEvidenceFailure {
        self.failure
    }

    /// Borrows the unchanged stored facts.
    pub const fn input(&self) -> &ReviewRunReconstitutionInput {
        &self.input
    }

    /// Returns the unchanged stored facts.
    pub fn into_input(self) -> ReviewRunReconstitutionInput {
        *self.input
    }
}

/// Why a review-run state cannot be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewRunTransitionFailure {
    /// Canonical pass evidence does not support the requested projection.
    Evidence(ReviewRunEvidenceFailure),
    /// The requested lifecycle edge is not permitted.
    InvalidTransition,
}

/// Rejected run transition retaining both states.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRunTransitionError {
    attempt: Box<ReviewRunTransitionAttempt>,
    failure: ReviewRunTransitionFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReviewRunTransitionAttempt {
    current: ReviewRunState,
    next: ReviewRunState,
    pass_evidence: Option<ReviewPassEvidence>,
}

impl ReviewRunTransitionError {
    /// Returns why the state was rejected.
    pub const fn failure(&self) -> ReviewRunTransitionFailure {
        self.failure
    }

    /// Returns the current and requested states.
    pub const fn states(&self) -> (ReviewRunState, ReviewRunState) {
        (self.attempt.current, self.attempt.next)
    }

    /// Returns the rejected canonical pass evidence.
    pub const fn pass_evidence(&self) -> Option<ReviewPassEvidence> {
        self.attempt.pass_evidence
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

/// Canonical session-turn lifecycle outcome projected into a review pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassTurnOutcome {
    /// The turn is active.
    Active,
    /// The turn completed successfully.
    Completed,
    /// The turn ended in a provider refusal.
    Refused,
    /// The turn failed.
    Failed,
    /// The turn was cancelled with authenticated authority.
    Cancelled,
    /// The turn requires external reconciliation.
    ReconciliationRequired,
}

/// Canonical turn facts independently loaded for one review pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPassTurnEvidence {
    turn: TurnId,
    session: SessionId,
    accepted_input: AcceptedInputId,
    outcome: ReviewPassTurnOutcome,
    terminal_frontier: Option<ContextFrontierId>,
}

impl ReviewPassTurnEvidence {
    /// Supplies the turn's independently stored ownership and terminal facts.
    pub const fn new(
        turn: TurnId,
        session: SessionId,
        accepted_input: AcceptedInputId,
        outcome: ReviewPassTurnOutcome,
        terminal_frontier: Option<ContextFrontierId>,
    ) -> Self {
        Self {
            turn,
            session,
            accepted_input,
            outcome,
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

    /// Returns the canonical turn lifecycle outcome.
    pub const fn outcome(self) -> ReviewPassTurnOutcome {
        self.outcome
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
    workflow: ReviewWorkflowKind,
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
        workflow: ReviewWorkflowKind,
        session: SessionId,
        accepted_input: AcceptedInputId,
        accepted_input_session: SessionId,
        state: ReviewPassState,
        turn_evidence: Option<ReviewPassTurnEvidence>,
    ) -> Self {
        Self {
            reference,
            kind,
            workflow,
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

    /// Returns the canonical parent run workflow.
    pub const fn workflow(self) -> ReviewWorkflowKind {
        self.workflow
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
        workflow: ReviewWorkflowKind,
        session: SessionId,
        accepted_input: AcceptedInputId,
        accepted_input_session: SessionId,
    ) -> Result<Self, ReviewPassConstructionError> {
        let failure = if !workflow_matches_pass_kind(workflow, kind) {
            Some(ReviewPassConstructionFailure::RunWorkflowMismatch)
        } else if session != accepted_input_session {
            Some(ReviewPassConstructionFailure::AcceptedInputSessionMismatch)
        } else {
            None
        };
        if let Some(failure) = failure {
            return Err(ReviewPassConstructionError {
                reference,
                kind,
                workflow,
                session,
                accepted_input,
                accepted_input_session,
                failure,
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

    /// Applies one permitted transition without changing the pass turn and only
    /// when canonical turn evidence supports the requested projection.
    pub fn transition(
        mut self,
        next: ReviewPassState,
        turn_evidence: Option<ReviewPassTurnEvidence>,
    ) -> Result<Self, ReviewPassTransitionError> {
        if let Some(failure) =
            validate_pass_turn_evidence(self.session, self.accepted_input, next, turn_evidence)
        {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: self.state,
                    next,
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::Evidence(failure),
            });
        }
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
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: self.state,
                    next,
                    turn_evidence,
                }),
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
    if !workflow_matches_pass_kind(input.workflow, input.kind) {
        return Some(ReviewPassReconstitutionFailure::RunWorkflowMismatch);
    }
    if input.session != input.accepted_input_session {
        return Some(ReviewPassReconstitutionFailure::AcceptedInputSessionMismatch);
    }
    validate_pass_turn_evidence(
        input.session,
        input.accepted_input,
        input.state,
        input.turn_evidence,
    )
}

fn validate_pass_turn_evidence(
    session: SessionId,
    accepted_input: AcceptedInputId,
    state: ReviewPassState,
    turn_evidence: Option<ReviewPassTurnEvidence>,
) -> Option<ReviewPassReconstitutionFailure> {
    let Some(turn) = state.turn() else {
        return turn_evidence
            .is_some()
            .then_some(ReviewPassReconstitutionFailure::UnexpectedTurnEvidence);
    };
    let Some(evidence) = turn_evidence else {
        return Some(ReviewPassReconstitutionFailure::MissingTurnEvidence);
    };
    if evidence.turn != turn {
        return Some(ReviewPassReconstitutionFailure::TurnMismatch);
    }
    if evidence.session != session {
        return Some(ReviewPassReconstitutionFailure::TurnSessionMismatch);
    }
    if evidence.accepted_input != accepted_input {
        return Some(ReviewPassReconstitutionFailure::TurnAcceptedInputMismatch);
    }
    if !pass_state_matches_turn_outcome(state, evidence.outcome) {
        return Some(ReviewPassReconstitutionFailure::TurnOutcomeMismatch);
    }
    if let ReviewPassState::Succeeded {
        output_frontier, ..
    } = state
        && evidence.terminal_frontier != Some(output_frontier)
    {
        return Some(ReviewPassReconstitutionFailure::OutputFrontierMismatch);
    }
    None
}

fn pass_state_matches_turn_outcome(state: ReviewPassState, outcome: ReviewPassTurnOutcome) -> bool {
    matches!(
        (state, outcome),
        (
            ReviewPassState::Running { .. },
            ReviewPassTurnOutcome::Active
                | ReviewPassTurnOutcome::Completed
                | ReviewPassTurnOutcome::Refused
                | ReviewPassTurnOutcome::Failed
                | ReviewPassTurnOutcome::Cancelled
                | ReviewPassTurnOutcome::ReconciliationRequired
        ) | (
            ReviewPassState::Succeeded { .. },
            ReviewPassTurnOutcome::Completed
        ) | (
            ReviewPassState::Failed { .. },
            ReviewPassTurnOutcome::Failed | ReviewPassTurnOutcome::Refused
        ) | (
            ReviewPassState::Blocked { .. },
            ReviewPassTurnOutcome::ReconciliationRequired
        ) | (
            ReviewPassState::Cancelled { turn: Some(_) },
            ReviewPassTurnOutcome::Cancelled
        )
    )
}

/// Why queued-pass construction was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassConstructionFailure {
    /// The pass kind is incompatible with its canonical run workflow.
    RunWorkflowMismatch,
    /// The accepted input belongs to another session.
    AcceptedInputSessionMismatch,
}

/// Rejected queued-pass construction retaining all supplied facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPassConstructionError {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    workflow: ReviewWorkflowKind,
    session: SessionId,
    accepted_input: AcceptedInputId,
    accepted_input_session: SessionId,
    failure: ReviewPassConstructionFailure,
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

    /// Returns the canonical parent workflow supplied for the pass.
    pub const fn workflow(self) -> ReviewWorkflowKind {
        self.workflow
    }

    /// Returns the pass and canonical accepted-input sessions.
    pub const fn sessions(self) -> (SessionId, SessionId) {
        (self.session, self.accepted_input_session)
    }

    /// Returns the accepted input whose association did not match.
    pub const fn accepted_input(self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns why construction was rejected.
    pub const fn failure(self) -> ReviewPassConstructionFailure {
        self.failure
    }
}

/// Why stored pass facts cannot reconstruct one canonical pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassReconstitutionFailure {
    /// The pass kind is incompatible with its canonical run workflow.
    RunWorkflowMismatch,
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
    /// The canonical turn lifecycle outcome differs from the pass projection.
    TurnOutcomeMismatch,
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
    /// Canonical turn evidence does not support the requested projection.
    Evidence(ReviewPassReconstitutionFailure),
    /// The requested lifecycle edge is not permitted.
    InvalidTransition,
    /// The transition names a different turn.
    TurnChanged,
}

/// Rejected pass transition retaining both states.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassTransitionError {
    attempt: Box<ReviewPassTransitionAttempt>,
    failure: ReviewPassTransitionFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReviewPassTransitionAttempt {
    current: ReviewPassState,
    next: ReviewPassState,
    turn_evidence: Option<ReviewPassTurnEvidence>,
}

impl ReviewPassTransitionError {
    /// Returns why the transition was rejected.
    pub const fn failure(&self) -> ReviewPassTransitionFailure {
        self.failure
    }

    /// Returns the current and requested states.
    pub const fn states(&self) -> (ReviewPassState, ReviewPassState) {
        (self.attempt.current, self.attempt.next)
    }

    /// Returns the rejected canonical turn evidence.
    pub const fn turn_evidence(&self) -> Option<ReviewPassTurnEvidence> {
        self.attempt.turn_evidence
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
    diff_side: Option<ReviewFindingDiffSide>,
}

impl ReviewFindingLocation {
    /// Constructs an exact path, optional line range, and optional diff side.
    pub const fn new(
        file_path: ReviewKey,
        line_range: Option<ReviewLineRange>,
        diff_side: Option<ReviewFindingDiffSide>,
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

    /// Returns the optional diff side.
    pub const fn diff_side(&self) -> Option<ReviewFindingDiffSide> {
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
    producing_pass: ReviewPassEvidence,
    content: ReviewFindingContent,
}

impl ReviewFindingProposal {
    /// Constructs a proposal against canonical target evidence and a producing
    /// pass in the same exact run.
    pub fn try_new(
        reference: ReviewFindingRef,
        producing_pass: ReviewPassEvidence,
        target: &ReviewTarget,
        content: ReviewFindingContent,
    ) -> Result<Self, ReviewFindingTransitionError> {
        if target.id() != reference.target() {
            return Err(ReviewFindingTransitionError::proposal(
                ReviewFindingTransitionFailure::ForeignTarget,
            ));
        }
        if content.location().diff_side().is_some() && target.base_revision().is_none() {
            return Err(ReviewFindingTransitionError::proposal(
                ReviewFindingTransitionFailure::MissingDiffBase,
            ));
        }
        if producing_pass.reference() != reference.pass() {
            return Err(ReviewFindingTransitionError::proposal(
                ReviewFindingTransitionFailure::ForeignProducingPass,
            ));
        }
        if producing_pass.kind() != ReviewPassKind::ReadOnlyReview
            || !matches!(producing_pass.state(), ReviewPassState::Succeeded { .. })
        {
            return Err(ReviewFindingTransitionError::proposal(
                ReviewFindingTransitionFailure::IncompatibleProducingPassEvidence,
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
    pub const fn producing_pass(&self) -> ReviewPassEvidence {
        self.producing_pass
    }

    /// Borrows immutable finding content.
    pub const fn content(&self) -> &ReviewFindingContent {
        &self.content
    }
}

/// A finding-associated external-link reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewFindingExternalLinkRef {
    finding: ReviewFindingRef,
    link: ReviewExternalLinkId,
    attachment_pass: ReviewPassEvidence,
}

impl ReviewFindingExternalLinkRef {
    /// Derives a finding-bound reference from one attached canonical link.
    #[allow(clippy::result_large_err)]
    pub fn try_new(
        finding: ReviewFindingRef,
        link: &ReviewExternalLink,
    ) -> Result<Self, ReviewFindingExternalLinkError> {
        if link.association() != ReviewExternalLinkAssociation::Finding(finding) {
            return Err(ReviewFindingExternalLinkError {
                finding,
                link: link.id(),
                association: link.association(),
                failure: ReviewFindingExternalLinkFailure::ForeignAssociation,
            });
        }
        let Some(attachment) = link.attachment() else {
            return Err(ReviewFindingExternalLinkError {
                finding,
                link: link.id(),
                association: link.association(),
                failure: ReviewFindingExternalLinkFailure::NotAttached,
            });
        };
        Ok(Self {
            finding,
            link: link.id(),
            attachment_pass: attachment.pass_evidence(),
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

    /// Returns the canonical pass that produced the attachment.
    pub const fn attachment_pass(self) -> ReviewPassEvidence {
        self.attachment_pass
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
    finding: ReviewFindingRef,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassEvidence,
    kind: ReviewFindingEventKind,
}

impl ReviewFindingEvent {
    /// Constructs one typed event.
    pub const fn new(
        finding: ReviewFindingRef,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassEvidence,
        kind: ReviewFindingEventKind,
    ) -> Self {
        Self {
            finding,
            ordinal,
            pass,
            kind,
        }
    }

    /// Returns the finding that owns this event.
    pub const fn finding(&self) -> ReviewFindingRef {
        self.finding
    }

    /// Returns the contiguous one-based ordinal.
    pub const fn ordinal(&self) -> ReviewEventOrdinal {
        self.ordinal
    }

    /// Returns the producing pass.
    pub const fn pass(&self) -> ReviewPassRef {
        self.pass.reference()
    }

    /// Returns the canonical producing-pass evidence.
    pub const fn pass_evidence(&self) -> ReviewPassEvidence {
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
        if event.finding != self.proposal.reference {
            return Err(ReviewFindingTransitionError {
                event: Some(Box::new(event)),
                failure: ReviewFindingTransitionFailure::ForeignEventFinding,
            });
        }
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
        if event.pass.reference().target() != self.proposal.reference.target() {
            return Err(ReviewFindingTransitionError {
                event: Some(Box::new(event)),
                failure: ReviewFindingTransitionFailure::ForeignEventPass,
            });
        }
        if !finding_event_matches_pass_evidence(&event.kind, event.pass) {
            return Err(ReviewFindingTransitionError {
                event: Some(Box::new(event)),
                failure: ReviewFindingTransitionFailure::IncompatibleEventPassEvidence,
            });
        }
        validate_finding_reference(&self.proposal, &event)?;
        let Some(next_status) = finding_transition(self.status, &event.kind, self.events.last())
        else {
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
            if link.attachment_pass() != event.pass_evidence() {
                return Err(ReviewFindingTransitionError {
                    event: Some(Box::new(event.clone())),
                    failure: ReviewFindingTransitionFailure::PublicationPassMismatch,
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
    previous: Option<&ReviewFindingEvent>,
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
        (Status::BlockedWithReason, Event::Posted { .. })
            if previous.is_some_and(|event| {
                matches!(event.kind(), Event::BlockedWithReason { .. })
                    && event.pass_evidence().kind() == ReviewPassKind::Publish
            }) =>
        {
            Some(Status::Posted)
        }
        (Status::Accepted | Status::Posted | Status::BlockedWithReason, Event::Fixed) => {
            Some(Status::Fixed)
        }
        (Status::Accepted | Status::Posted, Event::BlockedWithReason { .. }) => {
            Some(Status::BlockedWithReason)
        }
        _ => None,
    }
}

fn finding_event_matches_pass_evidence(
    event: &ReviewFindingEventKind,
    pass: ReviewPassEvidence,
) -> bool {
    let kind_matches = matches!(
        (event, pass.kind()),
        (
            ReviewFindingEventKind::Accepted
                | ReviewFindingEventKind::Rejected { .. }
                | ReviewFindingEventKind::Stale,
            ReviewPassKind::Judge
        ) | (
            ReviewFindingEventKind::Duplicate { .. } | ReviewFindingEventKind::Superseded { .. },
            ReviewPassKind::Dedupe
        ) | (
            ReviewFindingEventKind::Posted { .. },
            ReviewPassKind::Publish
        ) | (ReviewFindingEventKind::Fixed, ReviewPassKind::Fix)
            | (
                ReviewFindingEventKind::BlockedWithReason { .. },
                ReviewPassKind::Publish | ReviewPassKind::Fix
            )
    );
    let outcome_matches = if matches!(event, ReviewFindingEventKind::BlockedWithReason { .. }) {
        matches!(pass.state(), ReviewPassState::Blocked { .. })
    } else {
        matches!(pass.state(), ReviewPassState::Succeeded { .. })
    };
    kind_matches && outcome_matches
}

/// Why a finding proposal, event, or complete history is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewFindingTransitionFailure {
    /// The canonical target evidence belongs to another target.
    ForeignTarget,
    /// A diff-relative finding's canonical target has no comparison revision.
    MissingDiffBase,
    /// The producing pass does not belong to the finding's exact run.
    ForeignProducingPass,
    /// The producing pass did not canonically succeed as read-only review.
    IncompatibleProducingPassEvidence,
    /// An event belongs to another finding.
    ForeignEventFinding,
    /// An event pass belongs to another target.
    ForeignEventPass,
    /// The event kind or outcome cannot be produced by the canonical pass.
    IncompatibleEventPassEvidence,
    /// A duplicate or successor finding belongs to another run.
    ForeignReferencedFinding,
    /// A finding names itself as its canonical or successor finding.
    SelfReference,
    /// A publication link belongs to another finding.
    ForeignExternalLink,
    /// A posted event names a pass other than the attachment producer.
    PublicationPassMismatch,
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
    link: ReviewExternalLinkId,
    pass: ReviewPassEvidence,
    external_object: ReviewKey,
}

impl ReviewExternalLinkAttachment {
    /// Constructs attachment evidence.
    pub const fn new(
        link: ReviewExternalLinkId,
        pass: ReviewPassEvidence,
        external_object: ReviewKey,
    ) -> Self {
        Self {
            link,
            pass,
            external_object,
        }
    }

    /// Returns the owning external-link reservation.
    pub const fn link(&self) -> ReviewExternalLinkId {
        self.link
    }

    /// Returns the producing pass.
    pub const fn pass(&self) -> ReviewPassRef {
        self.pass.reference()
    }

    /// Returns the canonical producing-pass evidence.
    pub const fn pass_evidence(&self) -> ReviewPassEvidence {
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
    link: ReviewExternalLinkId,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassEvidence,
    state: ReviewExternalObjectState,
}

impl ReviewExternalLinkObservation {
    /// Constructs one observation.
    pub const fn new(
        link: ReviewExternalLinkId,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassEvidence,
        state: ReviewExternalObjectState,
    ) -> Self {
        Self {
            link,
            ordinal,
            pass,
            state,
        }
    }

    /// Returns the observed external-link reservation.
    pub const fn link(self) -> ReviewExternalLinkId {
        self.link
    }

    /// Returns the contiguous one-based ordinal.
    pub const fn ordinal(self) -> ReviewEventOrdinal {
        self.ordinal
    }

    /// Returns the observing pass.
    pub const fn pass(self) -> ReviewPassRef {
        self.pass.reference()
    }

    /// Returns the canonical observing-pass evidence.
    pub const fn pass_evidence(self) -> ReviewPassEvidence {
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
        if attachment.link != self.id {
            return Err(ReviewExternalLinkTransitionError::ForeignAttachmentLink);
        }
        if attachment.pass.reference().target() != self.association.target() {
            return Err(ReviewExternalLinkTransitionError::ForeignPass);
        }
        if !matches!(
            attachment.pass.kind(),
            ReviewPassKind::Publish | ReviewPassKind::ImportExternalContext
        ) || !matches!(attachment.pass.state(), ReviewPassState::Succeeded { .. })
        {
            return Err(ReviewExternalLinkTransitionError::IncompatibleAttachmentPass);
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
        if observation.link != self.id {
            return Err(ReviewExternalLinkTransitionError::ForeignObservationLink);
        }
        if observation.pass.reference().target() != self.association.target() {
            return Err(ReviewExternalLinkTransitionError::ForeignPass);
        }
        if observation.pass.kind() != ReviewPassKind::ImportExternalContext
            || !matches!(observation.pass.state(), ReviewPassState::Succeeded { .. })
        {
            return Err(ReviewExternalLinkTransitionError::IncompatibleObservationPass);
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
    /// An attachment belongs to another external-link reservation.
    ForeignAttachmentLink,
    /// An observation belongs to another external-link reservation.
    ForeignObservationLink,
    /// Attachment or observation evidence belongs to another target.
    ForeignPass,
    /// Attachment evidence did not canonically succeed in an attaching pass.
    IncompatibleAttachmentPass,
    /// Observation evidence did not canonically succeed in an import pass.
    IncompatibleObservationPass,
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

    fn succeeded_pass(value: u128, kind: ReviewPassKind) -> ReviewPassEvidence {
        ReviewPassEvidence::new(
            pass_ref(value),
            kind,
            ReviewPassState::Succeeded {
                turn: turn_id(value + 100),
                output_frontier: frontier_id(value + 200),
            },
        )
    }

    fn blocked_pass(value: u128, kind: ReviewPassKind) -> ReviewPassEvidence {
        ReviewPassEvidence::new(
            pass_ref(value),
            kind,
            ReviewPassState::Blocked {
                turn: turn_id(value + 100),
            },
        )
    }

    fn finding_ref(value: u128) -> ReviewFindingRef {
        ReviewFindingRef::new(pass_ref(3), finding_id(value))
    }

    fn target_with_base() -> ReviewTarget {
        ReviewTarget::try_new(
            target_id(1),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("head"),
            Some(key("base")),
            None,
        )
        .expect("fixture target has complete comparison evidence")
    }

    fn target_without_base() -> ReviewTarget {
        ReviewTarget::try_new(
            target_id(1),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("head"),
            None,
            None,
        )
        .expect("standalone commit target may omit a comparison revision")
    }

    fn finding_content(diff_side: Option<ReviewFindingDiffSide>) -> ReviewFindingContent {
        ReviewFindingContent::new(
            ReviewFindingLocation::new(
                key("src/lib.rs"),
                Some(ReviewLineRange::try_new(4, 7).expect("ordered line range")),
                diff_side,
            ),
            text("Finding title"),
            text("Finding body"),
            ReviewFindingSeverity::High,
            ReviewConfidence::try_from_basis_points(8_500).expect("bounded confidence"),
            key("correctness"),
            Some(text("Apply the exact fix")),
        )
    }

    fn proposal() -> ReviewFindingProposal {
        let target = target_with_base();
        ReviewFindingProposal::try_new(
            finding_ref(10),
            succeeded_pass(3, ReviewPassKind::ReadOnlyReview),
            &target,
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect("producing pass belongs to the finding run")
    }

    fn attached_finding_link(
        finding: ReviewFindingRef,
        link: ReviewExternalLinkId,
    ) -> ReviewExternalLink {
        attached_finding_link_with_pass(finding, link, succeeded_pass(20, ReviewPassKind::Publish))
    }

    fn attached_finding_link_with_pass(
        finding: ReviewFindingRef,
        link: ReviewExternalLinkId,
        pass: ReviewPassEvidence,
    ) -> ReviewExternalLink {
        ReviewExternalLink::reserve(
            link,
            ReviewExternalLinkAssociation::Finding(finding),
            key("code-host"),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(ReviewExternalLinkAttachment::new(
            link,
            pass,
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

    fn finding_link_ref_with_pass(
        finding: ReviewFindingRef,
        link: ReviewExternalLinkId,
        pass: ReviewPassEvidence,
    ) -> ReviewFindingExternalLinkRef {
        ReviewFindingExternalLinkRef::try_new(
            finding,
            &attached_finding_link_with_pass(finding, link, pass),
        )
        .expect("fixture link is attached by the exact pass")
    }

    #[track_caller]
    fn assert_pass_reconstitution_rejects(
        input: ReviewPassReconstitutionInput,
        expected: ReviewPassReconstitutionFailure,
    ) {
        let error = ReviewPass::try_reconstitute(input)
            .expect_err("cross-wired canonical pass evidence must fail closed");
        assert_eq!(error.failure(), expected);
        assert_eq!(*error.input(), input);
    }

    #[track_caller]
    fn assert_pass_outcome_reconstitutes(
        state: ReviewPassState,
        outcome: ReviewPassTurnOutcome,
        terminal_frontier: Option<ContextFrontierId>,
    ) {
        let input = ReviewPassReconstitutionInput::new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            ReviewWorkflowKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            session_id(4),
            state,
            Some(ReviewPassTurnEvidence::new(
                state.turn().expect("fixture state names a turn"),
                session_id(4),
                accepted_input_id(5),
                outcome,
                terminal_frontier,
            )),
        );
        assert_eq!(
            ReviewPass::try_reconstitute(input)
                .expect("canonical turn outcome supports the pass")
                .state(),
            state
        );
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

    fn finding_transition_rows() -> Vec<FindingTransitionRow> {
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
            [false, false, false, true, true, true, true, false],
        ];
        statuses
            .into_iter()
            .enumerate()
            .map(|(status_index, current)| {
                let permitted_events = events
                    .iter()
                    .enumerate()
                    .filter_map(|(event_index, (name, event, next))| {
                        let previous =
                            (current == ReviewFindingStatus::BlockedWithReason).then(|| {
                                ReviewFindingEvent::new(
                                    finding_ref(10),
                                    ReviewEventOrdinal::one(),
                                    blocked_pass(19, ReviewPassKind::Publish),
                                    ReviewFindingEventKind::BlockedWithReason {
                                        reason: text("pending acknowledgement"),
                                    },
                                )
                            });
                        let actual = finding_transition(current, event, previous.as_ref());
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
            .collect()
    }

    /// S29 / INV-040: change-request review freezes its comparison revision.
    #[test]
    fn s29_inv040_change_request_target_requires_frozen_base_revision() {
        let error = ReviewTarget::try_new(
            target_id(1),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::ChangeRequest(
                ReviewChangeRequestNumber::try_new(42).expect("positive change-request number"),
            ),
            key("head"),
            None,
            None,
        )
        .expect_err("change-request comparison revision must be frozen");
        assert_eq!(
            error,
            ReviewTargetError::MissingChangeRequestBase {
                target: target_id(1)
            }
        );
    }

    /// INV-040: a stack parent remains inside the child's exact repository
    /// topology.
    #[test]
    fn inv040_review_target_rejects_foreign_stack_parent() {
        let error = ReviewTarget::try_new(
            target_id(1),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("head"),
            None,
            Some(ReviewTargetParentRef::new(
                target_id(2),
                key("other-host"),
                key("repository"),
            )),
        )
        .expect_err("stack parent must share the child's provider and repository");
        assert_eq!(
            error,
            ReviewTargetError::ForeignParent {
                target: target_id(1)
            }
        );
    }

    /// INV-001: review identities remain distinct while composite references
    /// preserve exact ancestry.
    #[test]
    fn inv001_review_references_preserve_typed_identity_ancestry() {
        let run = ReviewRunRef::new(target_id(1), run_id(2));
        let pass = ReviewPassRef::new(run, pass_id(3));
        let finding = ReviewFindingRef::new(pass, finding_id(4));

        assert_eq!(run.target(), target_id(1));
        assert_eq!(run.run(), run_id(2));
        assert_eq!(pass.run(), run);
        assert_eq!(pass.pass(), pass_id(3));
        assert_eq!(finding.pass(), pass);
        assert_eq!(finding.run(), run);
        assert_eq!(finding.finding(), finding_id(4));
    }

    /// S29 / INV-040: diff-relative locations require a frozen comparison.
    #[test]
    fn s29_inv040_diff_relative_finding_requires_target_comparison_revision() {
        let target = target_without_base();
        let error = ReviewFindingProposal::try_new(
            finding_ref(10),
            succeeded_pass(3, ReviewPassKind::ReadOnlyReview),
            &target,
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect_err("diff side has no stable meaning without a base revision");
        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::MissingDiffBase
        );
    }

    /// S29 / INV-040: standalone commit review may remain file-relative.
    #[test]
    fn s29_inv040_file_relative_finding_allows_standalone_commit_target() {
        let target = target_without_base();
        let proposal = ReviewFindingProposal::try_new(
            finding_ref(10),
            succeeded_pass(3, ReviewPassKind::ReadOnlyReview),
            &target,
            finding_content(None),
        )
        .expect("file-relative finding needs no comparison revision");
        assert_eq!(proposal.reference(), finding_ref(10));
        assert_eq!(proposal.content().location().diff_side(), None);
    }

    /// INV-040: a finding reference authenticates its exact producing pass, not
    /// merely another pass in the same run.
    #[test]
    fn inv040_finding_reference_rejects_cross_wired_producing_pass() {
        let target = target_with_base();
        let error = ReviewFindingProposal::try_new(
            finding_ref(10),
            succeeded_pass(4, ReviewPassKind::ReadOnlyReview),
            &target,
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect_err("finding reference must name its exact producing pass");
        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::ForeignProducingPass
        );
    }

    /// INV-040: immutable finding content requires a canonically successful
    /// read-only-review producer.
    #[test]
    fn inv040_finding_proposal_rejects_incompatible_producing_pass() {
        let target = target_with_base();
        let error = ReviewFindingProposal::try_new(
            finding_ref(10),
            ReviewPassEvidence::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewPassState::Failed { turn: turn_id(6) },
            ),
            &target,
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect_err("failed review pass cannot produce durable finding content");
        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleProducingPassEvidence
        );
    }

    /// INV-040: run transitions reject a pass owned by another run.
    #[test]
    fn inv040_run_transition_rejects_foreign_pass() {
        let reference = run_ref();
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
            .transition(
                requested,
                Some(ReviewPassEvidence::new(
                    foreign,
                    ReviewPassKind::ReadOnlyReview,
                    ReviewPassState::Running { turn: turn_id(6) },
                )),
            )
            .expect_err("foreign pass must fail closed");
        assert_eq!(
            cross_wired.failure(),
            ReviewRunTransitionFailure::Evidence(ReviewRunEvidenceFailure::ForeignPass)
        );
        assert_eq!(cross_wired.states(), (ReviewRunState::Queued, requested));
    }

    /// INV-040: a run admits only the pass kind corresponding to its workflow.
    #[test]
    fn inv040_run_transition_rejects_incompatible_pass_kind() {
        let queued = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let requested = ReviewRunState::Running {
            active_pass: pass_ref(3),
        };
        let error = queued
            .transition(
                requested,
                Some(ReviewPassEvidence::new(
                    pass_ref(3),
                    ReviewPassKind::Publish,
                    ReviewPassState::Running { turn: turn_id(6) },
                )),
            )
            .expect_err("publication pass cannot execute a read-only-review run");
        assert_eq!(
            error.failure(),
            ReviewRunTransitionFailure::Evidence(ReviewRunEvidenceFailure::PassKindMismatch)
        );
    }

    /// INV-040: queued pass construction authenticates its accepted-input
    /// session.
    #[test]
    fn inv040_pass_construction_rejects_foreign_accepted_input_session() {
        let cross_wired_input = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            ReviewWorkflowKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            session_id(99),
        )
        .expect_err("accepted input must belong to the pass session");
        assert_eq!(
            cross_wired_input.sessions(),
            (session_id(4), session_id(99))
        );
    }

    /// INV-040: queued-pass construction authenticates its parent workflow.
    #[test]
    fn inv040_pass_construction_rejects_incompatible_run_workflow() {
        let error = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::Publish,
            ReviewWorkflowKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            session_id(4),
        )
        .expect_err("pass kind must correspond to its canonical run workflow");
        assert_eq!(
            error.failure(),
            ReviewPassConstructionFailure::RunWorkflowMismatch
        );
    }

    /// INV-040: one active pass turn remains fixed through terminalization.
    #[test]
    fn inv040_pass_transition_rejects_changed_turn() {
        let running = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            ReviewWorkflowKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            session_id(4),
        )
        .expect("accepted input belongs to the pass session")
        .transition(
            ReviewPassState::Running { turn: turn_id(6) },
            Some(ReviewPassTurnEvidence::new(
                turn_id(6),
                session_id(4),
                accepted_input_id(5),
                ReviewPassTurnOutcome::Active,
                None,
            )),
        )
        .expect("queued pass may activate");
        let changed_turn = running
            .transition(
                ReviewPassState::Succeeded {
                    turn: turn_id(7),
                    output_frontier: frontier_id(8),
                },
                Some(ReviewPassTurnEvidence::new(
                    turn_id(7),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            )
            .expect_err("terminal evidence must retain the active turn");
        assert_eq!(
            changed_turn.failure(),
            ReviewPassTransitionFailure::TurnChanged
        );
    }

    /// S29 / INV-040: a running pass admits monotonic lag after its canonical
    /// turn terminalizes.
    #[test]
    fn s29_inv040_running_pass_admits_terminal_turn_projection_lag() {
        let queued = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            ReviewWorkflowKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            session_id(4),
        )
        .expect("accepted input belongs to the pass session");
        let requested = ReviewPassState::Running { turn: turn_id(6) };
        let running = queued
            .transition(
                requested,
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            )
            .expect("terminal turn evidence may lead a lagging running pass");
        assert_eq!(running.state(), requested);
    }

    /// S29 / INV-040: run reconstitution authenticates its state against the
    /// canonical referenced pass outcome.
    #[test]
    fn s29_inv040_run_reconstitution_rejects_cross_wired_pass_outcome() {
        let state = ReviewRunState::Succeeded {
            concluding_pass: pass_ref(3),
        };
        let exact = ReviewRunReconstitutionInput::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            state,
            Some(ReviewPassEvidence::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                },
            )),
        );
        assert_eq!(
            ReviewRun::try_reconstitute(exact)
                .expect("canonical pass outcome supports the run")
                .state(),
            state
        );

        let mismatched = ReviewRunReconstitutionInput::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            state,
            Some(ReviewPassEvidence::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewPassState::Failed { turn: turn_id(6) },
            )),
        );
        let mismatch = ReviewRun::try_reconstitute(mismatched)
            .expect_err("a failed pass cannot support a succeeded run");
        assert_eq!(
            mismatch.failure(),
            ReviewRunEvidenceFailure::PassStateMismatch
        );
        assert_eq!(mismatch.input(), &mismatched);
    }

    /// S29 / INV-040: a run that names a pass requires independently loaded
    /// canonical pass evidence.
    #[test]
    fn s29_inv040_run_reconstitution_requires_pass_evidence() {
        let state = ReviewRunState::Succeeded {
            concluding_pass: pass_ref(3),
        };
        let missing = ReviewRunReconstitutionInput::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            state,
            None,
        );
        let missing_error = ReviewRun::try_reconstitute(missing)
            .expect_err("a concluding run requires canonical pass evidence");
        assert_eq!(
            missing_error.failure(),
            ReviewRunEvidenceFailure::MissingPassEvidence
        );
        assert_eq!(missing_error.input(), &missing);
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
            ReviewPassTurnOutcome::Completed,
            Some(frontier_id(8)),
        );
        let exact = ReviewPassReconstitutionInput::new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            ReviewWorkflowKind::ReadOnlyReview,
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

        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                session_id(9),
                state,
                Some(exact_turn),
            ),
            ReviewPassReconstitutionFailure::AcceptedInputSessionMismatch,
        );
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                session_id(4),
                state,
                None,
            ),
            ReviewPassReconstitutionFailure::MissingTurnEvidence,
        );
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                session_id(4),
                state,
                Some(ReviewPassTurnEvidence::new(
                    turn_id(7),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            ),
            ReviewPassReconstitutionFailure::TurnMismatch,
        );
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                session_id(4),
                state,
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(9),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            ),
            ReviewPassReconstitutionFailure::TurnSessionMismatch,
        );
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                session_id(4),
                state,
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(9),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            ),
            ReviewPassReconstitutionFailure::TurnAcceptedInputMismatch,
        );
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                session_id(4),
                state,
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Failed,
                    Some(frontier_id(8)),
                )),
            ),
            ReviewPassReconstitutionFailure::TurnOutcomeMismatch,
        );
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                session_id(4),
                state,
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(9)),
                )),
            ),
            ReviewPassReconstitutionFailure::OutputFrontierMismatch,
        );
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                session_id(4),
                ReviewPassState::Queued,
                Some(exact_turn),
            ),
            ReviewPassReconstitutionFailure::UnexpectedTurnEvidence,
        );
    }

    /// S29 / INV-040: every evidence-bearing pass projection agrees with the
    /// canonical session-turn outcome.
    #[test]
    fn s29_inv040_pass_reconstitution_accepts_corresponding_turn_outcomes() {
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Running { turn: turn_id(6) },
            ReviewPassTurnOutcome::Active,
            None,
        );
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Failed { turn: turn_id(6) },
            ReviewPassTurnOutcome::Failed,
            None,
        );
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Failed { turn: turn_id(6) },
            ReviewPassTurnOutcome::Refused,
            None,
        );
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Blocked { turn: turn_id(6) },
            ReviewPassTurnOutcome::ReconciliationRequired,
            None,
        );
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Cancelled {
                turn: Some(turn_id(6)),
            },
            ReviewPassTurnOutcome::Cancelled,
            None,
        );
    }

    /// INV-040 / INV-041: a pending reservation is not posting evidence.
    #[test]
    fn inv040_posted_link_rejects_pending_reservation() {
        let finding = finding_ref(10);
        let pending = ReviewExternalLink::reserve(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding),
            key("code-host"),
            ReviewExternalObjectKind::ReviewComment,
        );

        let error = ReviewFindingExternalLinkRef::try_new(finding, &pending)
            .expect_err("pending reservation is not posting evidence");

        assert_eq!(
            error.failure(),
            ReviewFindingExternalLinkFailure::NotAttached
        );
    }

    /// INV-040 / INV-041: an attached link canonically associated with another
    /// finding is not posting evidence for this finding.
    #[test]
    fn inv040_posted_link_rejects_foreign_canonical_association() {
        let finding = finding_ref(10);
        let foreign = attached_finding_link(finding_ref(11), link_id(31));

        let error = ReviewFindingExternalLinkRef::try_new(finding, &foreign)
            .expect_err("canonical association belongs to another finding");

        assert_eq!(
            error.failure(),
            ReviewFindingExternalLinkFailure::ForeignAssociation
        );
        assert_eq!(
            error.association(),
            ReviewExternalLinkAssociation::Finding(finding_ref(11))
        );
    }

    /// INV-040 / INV-041: a posted finding consumes an attached canonical link
    /// associated with that exact finding.
    #[test]
    fn inv040_posted_link_accepts_exact_attached_association() {
        let finding = finding_ref(10);
        let exact = attached_finding_link(finding, link_id(32));

        let posted = ReviewFindingExternalLinkRef::try_new(finding, &exact)
            .expect("attached canonical association supports posting");

        assert_eq!(posted.link(), link_id(32));
    }

    /// INV-040: a terminal finding cannot reopen.
    #[test]
    fn inv040_finding_machine_rejects_terminal_reopening() {
        let finding = ReviewFinding::new(proposal())
            .apply(ReviewFindingEvent::new(
                finding_ref(10),
                ReviewEventOrdinal::one(),
                succeeded_pass(19, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ))
            .expect("open finding may be accepted")
            .apply(ReviewFindingEvent::new(
                finding_ref(10),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                succeeded_pass(20, ReviewPassKind::Publish),
                ReviewFindingEventKind::Posted {
                    link: finding_link_ref(finding_ref(10), link_id(30)),
                },
            ))
            .expect("accepted finding may be posted")
            .apply(ReviewFindingEvent::new(
                finding_ref(10),
                ReviewEventOrdinal::try_new(3).expect("positive ordinal"),
                succeeded_pass(22, ReviewPassKind::Fix),
                ReviewFindingEventKind::Fixed,
            ))
            .expect("posted finding may be fixed");
        assert_eq!(finding.status(), ReviewFindingStatus::Fixed);

        let reopened = finding
            .apply(ReviewFindingEvent::new(
                finding_ref(10),
                ReviewEventOrdinal::try_new(4).expect("positive ordinal"),
                succeeded_pass(23, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ))
            .expect_err("fixed finding is terminal");
        assert_eq!(
            reopened.failure(),
            ReviewFindingTransitionFailure::InvalidTransition {
                current: ReviewFindingStatus::Fixed
            }
        );
    }

    /// INV-040: finding history begins at ordinal one.
    #[test]
    fn inv040_finding_history_rejects_noncontiguous_first_ordinal() {
        let gap = ReviewFinding::new(proposal())
            .apply(ReviewFindingEvent::new(
                finding_ref(10),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                succeeded_pass(20, ReviewPassKind::Judge),
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

    /// S29 / INV-040: an event cannot be replayed into another same-run
    /// finding.
    #[test]
    fn s29_inv040_finding_history_rejects_foreign_event_owner() {
        let event = ReviewFindingEvent::new(
            finding_ref(11),
            ReviewEventOrdinal::one(),
            succeeded_pass(20, ReviewPassKind::Judge),
            ReviewFindingEventKind::Accepted,
        );
        let error = ReviewFinding::new(proposal())
            .apply(event.clone())
            .expect_err("event owner must match the aggregate finding");
        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::ForeignEventFinding
        );
        assert_eq!(error.event(), Some(&event));
    }

    /// INV-040: a same-target pass cannot produce an event outside its closed
    /// pass-kind responsibility.
    #[test]
    fn inv040_finding_history_rejects_incompatible_event_pass_kind() {
        let event = ReviewFindingEvent::new(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            succeeded_pass(20, ReviewPassKind::Publish),
            ReviewFindingEventKind::Accepted,
        );
        let error = ReviewFinding::new(proposal())
            .apply(event.clone())
            .expect_err("publication pass cannot accept a finding");
        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleEventPassEvidence
        );
        assert_eq!(error.event(), Some(&event));
    }

    /// INV-040: a compatible pass kind cannot author a finding event after a
    /// failed outcome.
    #[test]
    fn inv040_finding_history_rejects_incompatible_event_pass_outcome() {
        let event = ReviewFindingEvent::new(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            ReviewPassEvidence::new(
                pass_ref(20),
                ReviewPassKind::Judge,
                ReviewPassState::Failed { turn: turn_id(120) },
            ),
            ReviewFindingEventKind::Accepted,
        );
        let error = ReviewFinding::new(proposal())
            .apply(event)
            .expect_err("failed judgment cannot accept a finding");
        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleEventPassEvidence
        );
    }

    /// INV-040 / INV-041: publication is attributed to the pass that produced
    /// the attached external object.
    #[test]
    fn inv040_posted_event_rejects_another_publication_pass() {
        let event = ReviewFindingEvent::new(
            finding_ref(10),
            ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
            succeeded_pass(21, ReviewPassKind::Publish),
            ReviewFindingEventKind::Posted {
                link: finding_link_ref(finding_ref(10), link_id(30)),
            },
        );
        let error = ReviewFinding::new(proposal())
            .apply(ReviewFindingEvent::new(
                finding_ref(10),
                ReviewEventOrdinal::one(),
                succeeded_pass(19, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ))
            .expect("finding may be accepted")
            .apply(event)
            .expect_err("posting pass must equal the attachment producer");
        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::PublicationPassMismatch
        );
    }

    /// INV-040 / INV-041: reconciliation may post a publication-blocked
    /// finding once a later publication pass attaches the object.
    #[test]
    fn inv040_publication_blocked_finding_can_reconcile_to_posted() {
        let finding = ReviewFinding::new(proposal())
            .apply(ReviewFindingEvent::new(
                finding_ref(10),
                ReviewEventOrdinal::one(),
                succeeded_pass(19, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ))
            .expect("finding may be accepted")
            .apply(ReviewFindingEvent::new(
                finding_ref(10),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                blocked_pass(20, ReviewPassKind::Publish),
                ReviewFindingEventKind::BlockedWithReason {
                    reason: text("lost acknowledgement"),
                },
            ))
            .expect("blocked publication retains its reason")
            .apply(ReviewFindingEvent::new(
                finding_ref(10),
                ReviewEventOrdinal::try_new(3).expect("positive ordinal"),
                succeeded_pass(21, ReviewPassKind::Publish),
                ReviewFindingEventKind::Posted {
                    link: finding_link_ref_with_pass(
                        finding_ref(10),
                        link_id(31),
                        succeeded_pass(21, ReviewPassKind::Publish),
                    ),
                },
            ))
            .expect("confirmed attachment reconciles the publication");
        assert_eq!(finding.status(), ReviewFindingStatus::Posted);
    }

    /// INV-040: the complete nine-state finding transition surface stays
    /// closed and reviewable as one table.
    #[test]
    fn inv040_finding_transition_matrix_is_closed() {
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
            │ BlockedWithReason │ Superseded, Stale, Posted, Fixed                               │
            └───────────────────┴────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table(finding_transition_rows()));
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
                link_id(30),
                ReviewEventOrdinal::one(),
                succeeded_pass(20, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect_err("observation cannot prove an unattached effect");
        assert_eq!(premature, ReviewExternalLinkTransitionError::NotAttached);

        let attached = pending
            .attach(ReviewExternalLinkAttachment::new(
                link_id(30),
                succeeded_pass(20, ReviewPassKind::Publish),
                key("external-comment-42"),
            ))
            .expect("same-target pass may attach the reservation")
            .observe(ReviewExternalLinkObservation::new(
                link_id(30),
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect("attached link admits contiguous observations");
        assert_eq!(
            attached.attachment().map(|value| value.external_object()),
            Some(&key("external-comment-42"))
        );
    }

    /// INV-041: failed or operation-incompatible passes cannot attach external
    /// effect evidence.
    #[test]
    fn inv041_external_link_rejects_incompatible_attachment_pass() {
        let pending = ReviewExternalLink::reserve(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            key("code-host"),
            ReviewExternalObjectKind::ReviewComment,
        );
        let error = pending
            .attach(ReviewExternalLinkAttachment::new(
                link_id(30),
                succeeded_pass(20, ReviewPassKind::Judge),
                key("external-comment-42"),
            ))
            .expect_err("judgment pass cannot produce an external attachment");
        assert_eq!(
            error,
            ReviewExternalLinkTransitionError::IncompatibleAttachmentPass
        );
    }

    /// INV-041: external-state observations require successful import evidence.
    #[test]
    fn inv041_external_link_rejects_incompatible_observation_pass() {
        let attached = attached_finding_link(finding_ref(10), link_id(30));
        let error = attached
            .observe(ReviewExternalLinkObservation::new(
                link_id(30),
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::Judge),
                ReviewExternalObjectState::Current,
            ))
            .expect_err("judgment pass cannot author an external observation");
        assert_eq!(
            error,
            ReviewExternalLinkTransitionError::IncompatibleObservationPass
        );
    }

    /// INV-040: an observation from another same-target link cannot be
    /// attributed to the loaded aggregate.
    #[test]
    fn inv040_external_link_rejects_foreign_observation_owner() {
        let attached = ReviewExternalLink::reserve(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            key("code-host"),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(ReviewExternalLinkAttachment::new(
            link_id(30),
            succeeded_pass(20, ReviewPassKind::Publish),
            key("external-comment-42"),
        ))
        .expect("same-target pass may attach the reservation");
        let error = attached
            .observe(ReviewExternalLinkObservation::new(
                link_id(31),
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect_err("observation owner must match the aggregate link");
        assert_eq!(
            error,
            ReviewExternalLinkTransitionError::ForeignObservationLink
        );
    }

    /// INV-041: an attachment from another same-target reservation cannot be
    /// attributed to the loaded aggregate.
    #[test]
    fn inv041_external_link_rejects_foreign_attachment_owner() {
        let pending = ReviewExternalLink::reserve(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            key("code-host"),
            ReviewExternalObjectKind::ReviewComment,
        );
        let error = pending
            .attach(ReviewExternalLinkAttachment::new(
                link_id(31),
                succeeded_pass(20, ReviewPassKind::Publish),
                key("external-comment-42"),
            ))
            .expect_err("attachment owner must match the aggregate link");
        assert_eq!(
            error,
            ReviewExternalLinkTransitionError::ForeignAttachmentLink
        );
    }

    #[test]
    fn version_one_policy_has_exact_threshold_tuple() {
        assert_eq!(
            ReviewPolicy::version_one()
                .minimum_judge_confidence()
                .basis_points(),
            7_000
        );
        assert_eq!(
            ReviewPolicy::version_one()
                .minimum_publication_confidence()
                .basis_points(),
            8_000
        );
    }

    #[test]
    fn policy_rejects_publication_threshold_below_judgment() {
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
    }

    #[test]
    fn version_one_policy_rejects_noncanonical_tuple() {
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
    }

    #[test]
    fn later_policy_version_admits_ordered_threshold_tuple() {
        ReviewPolicy::try_new(
            ReviewPolicyVersion::try_new(2).expect("positive version"),
            ReviewConfidence::try_from_basis_points(7_000).expect("bounded confidence"),
            ReviewConfidence::try_from_basis_points(8_001).expect("bounded confidence"),
        )
        .expect("later versions admit their own ordered threshold tuples");
    }

    #[test]
    fn review_key_rejects_utf8_content_over_byte_budget() {
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
