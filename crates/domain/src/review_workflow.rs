//! Review-workflow aggregates above session execution.
//!
//! The normative specification is `docs/spec/review-workflows.md`.

mod reference;
pub use reference::{ReviewFindingRef, ReviewPassRef, ReviewRunRef};

mod target;
pub use target::{ReviewTarget, ReviewTargetError, ReviewTargetParentRef, ReviewTargetSubject};

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
};

use crate::{
    AcceptedInputId, ContextFrontierId, ReviewExternalLinkId, ReviewFindingId, ReviewPassId,
    ReviewRunId, ReviewTargetId, SessionId, TurnId,
};

const REVIEW_PRODUCED_FINDINGS_MAXIMUM: usize = 32;

mod policy;
mod value;

pub use policy::{ReviewPolicy, ReviewPolicyError, ReviewPolicyVersion};
pub use value::{
    ReviewChangeRequestNumber, ReviewConfidence, ReviewConfidenceError, ReviewEventOrdinal,
    ReviewFindingConfidenceAxes, ReviewKey, ReviewPositiveNumberError, ReviewText,
    ReviewValueError, ReviewValueFailure,
};

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

/// Closed discriminator committed by a finding-event pass result.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewFindingEventType {
    /// Accepted judgment.
    Accepted,
    /// Rejected judgment.
    Rejected,
    /// Duplicate classification.
    Duplicate,
    /// Supersession classification.
    Superseded,
    /// Stale classification.
    Stale,
    /// External publication.
    Posted,
    /// Successful repair.
    Fixed,
    /// Blocked publication or repair.
    BlockedWithReason,
}

/// Meaning-bearing finding-event payload committed by one pass result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewFindingEventResultKind {
    /// Accepted judgment.
    Accepted,
    /// Rejected judgment with its exact reason.
    Rejected {
        /// Exact rejection reason.
        reason: ReviewText,
    },
    /// Duplicate classification with authenticated canonical finding evidence.
    Duplicate {
        /// Canonical finding and its status at admission.
        canonical: ReviewReferencedFindingEvidence,
    },
    /// Supersession with authenticated successor evidence.
    Superseded {
        /// Successor finding and its status at admission.
        successor: ReviewReferencedFindingEvidence,
    },
    /// Stale classification.
    Stale,
    /// External publication through the exact reservation.
    Posted {
        /// External-link reservation consumed by the event.
        link: ReviewExternalLinkId,
    },
    /// Successful repair.
    Fixed,
    /// Blocked publication or repair with its exact reason.
    BlockedWithReason {
        /// Exact blocking reason.
        reason: ReviewText,
        /// Exact pending reservation attempted by publication; absent for repair.
        link: Option<ReviewExternalLinkId>,
    },
}

impl ReviewFindingEventResultKind {
    /// Returns the closed event discriminator.
    pub const fn event_type(&self) -> ReviewFindingEventType {
        match self {
            Self::Accepted => ReviewFindingEventType::Accepted,
            Self::Rejected { .. } => ReviewFindingEventType::Rejected,
            Self::Duplicate { .. } => ReviewFindingEventType::Duplicate,
            Self::Superseded { .. } => ReviewFindingEventType::Superseded,
            Self::Stale => ReviewFindingEventType::Stale,
            Self::Posted { .. } => ReviewFindingEventType::Posted,
            Self::Fixed => ReviewFindingEventType::Fixed,
            Self::BlockedWithReason { .. } => ReviewFindingEventType::BlockedWithReason,
        }
    }
}

/// Exact finding event committed by one pass result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFindingEventResult {
    finding: ReviewFindingRef,
    ordinal: ReviewEventOrdinal,
    kind: ReviewFindingEventResultKind,
}

impl ReviewFindingEventResult {
    /// Binds one terminal pass result to one exact finding and complete payload.
    pub fn new(
        finding: ReviewFindingRef,
        ordinal: ReviewEventOrdinal,
        kind: ReviewFindingEventResultKind,
    ) -> Self {
        Self {
            finding,
            ordinal,
            kind,
        }
    }

    /// Returns the owning finding.
    pub const fn finding(&self) -> ReviewFindingRef {
        self.finding
    }

    /// Returns the exact event ordinal.
    pub const fn ordinal(&self) -> ReviewEventOrdinal {
        self.ordinal
    }

    /// Returns the closed event discriminator.
    pub const fn event_type(&self) -> ReviewFindingEventType {
        self.kind.event_type()
    }

    /// Borrows the complete projected event payload.
    pub const fn kind(&self) -> &ReviewFindingEventResultKind {
        &self.kind
    }
}

/// Canonical bounded inventory of findings produced by one read-only pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewProducedFindings {
    findings: Vec<ReviewFindingRef>,
}

impl ReviewProducedFindings {
    /// Canonicalizes and bounds one exact finding inventory.
    pub fn try_new(
        mut findings: Vec<ReviewFindingRef>,
    ) -> Result<Self, ReviewProducedFindingsError> {
        if findings.len() > REVIEW_PRODUCED_FINDINGS_MAXIMUM {
            return Err(ReviewProducedFindingsError::TooMany {
                actual: findings.len(),
                maximum: REVIEW_PRODUCED_FINDINGS_MAXIMUM,
            });
        }
        findings.sort_unstable();
        if let Some(duplicate) = findings
            .windows(2)
            .find_map(|pair| (pair[0] == pair[1]).then_some(pair[0]))
        {
            return Err(ReviewProducedFindingsError::Duplicate { finding: duplicate });
        }
        Ok(Self { findings })
    }

    /// Borrows the canonical identity-ordered inventory.
    pub fn findings(&self) -> &[ReviewFindingRef] {
        &self.findings
    }

    /// Returns whether the exact finding is committed by this inventory.
    pub fn contains(&self, finding: ReviewFindingRef) -> bool {
        self.findings.binary_search(&finding).is_ok()
    }
}

/// Why a produced-finding inventory is not canonical.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewProducedFindingsError {
    /// The inventory exceeds the recorded admission budget.
    TooMany {
        /// Supplied finding count.
        actual: usize,
        /// Maximum admitted finding count.
        maximum: usize,
    },
    /// One finding identity appeared more than once.
    Duplicate {
        /// Duplicated complete finding reference.
        finding: ReviewFindingRef,
    },
}

/// Exact external attachment committed by one pass result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkAttachmentResult {
    link: ReviewExternalLinkId,
    external_object: ReviewKey,
    finding_event: Option<ReviewFindingEventResult>,
}

impl ReviewExternalLinkAttachmentResult {
    /// Commits one reservation, canonical object key, and optional posted event.
    pub const fn new(
        link: ReviewExternalLinkId,
        external_object: ReviewKey,
        finding_event: Option<ReviewFindingEventResult>,
    ) -> Self {
        Self {
            link,
            external_object,
            finding_event,
        }
    }

    /// Returns the exact reservation.
    pub const fn link(&self) -> ReviewExternalLinkId {
        self.link
    }

    /// Borrows the exact canonical external object key.
    pub const fn external_object(&self) -> &ReviewKey {
        &self.external_object
    }

    /// Borrows the exact posted event committed with the attachment.
    pub const fn finding_event(&self) -> Option<&ReviewFindingEventResult> {
        self.finding_event.as_ref()
    }
}

/// Exact external observation committed by one pass result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkObservationResult {
    link: ReviewExternalLinkId,
    ordinal: ReviewEventOrdinal,
    state: ReviewExternalObjectState,
}

impl ReviewExternalLinkObservationResult {
    /// Commits one reservation, observation ordinal, and reported state.
    pub const fn new(
        link: ReviewExternalLinkId,
        ordinal: ReviewEventOrdinal,
        state: ReviewExternalObjectState,
    ) -> Self {
        Self {
            link,
            ordinal,
            state,
        }
    }

    /// Returns the exact reservation.
    pub const fn link(self) -> ReviewExternalLinkId {
        self.link
    }

    /// Returns the exact observation ordinal.
    pub const fn ordinal(self) -> ReviewEventOrdinal {
        self.ordinal
    }

    /// Returns the exact observed state.
    pub const fn state(self) -> ReviewExternalObjectState {
        self.state
    }
}

/// Exact unchanged external state that consumed one import pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkNoChangeResult {
    link: ReviewExternalLinkId,
    observed_through: ReviewEventOrdinal,
    state: ReviewExternalObjectState,
}

impl ReviewExternalLinkNoChangeResult {
    /// Commits one reservation, consumed observation frontier, and unchanged state.
    pub const fn new(
        link: ReviewExternalLinkId,
        observed_through: ReviewEventOrdinal,
        state: ReviewExternalObjectState,
    ) -> Self {
        Self {
            link,
            observed_through,
            state,
        }
    }

    /// Returns the exact reservation.
    pub const fn link(self) -> ReviewExternalLinkId {
        self.link
    }

    /// Returns the exact latest observation consumed by this report.
    pub const fn observed_through(self) -> ReviewEventOrdinal {
        self.observed_through
    }

    /// Returns the exact unchanged reported state.
    pub const fn state(self) -> ReviewExternalObjectState {
        self.state
    }
}

/// Exact pending publication reservation consumed by one blocked pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkPublicationBlockedResult {
    link: ReviewExternalLinkId,
    reason: ReviewText,
}

impl ReviewExternalLinkPublicationBlockedResult {
    /// Commits one pending reservation and nonempty blocking reason.
    pub const fn new(link: ReviewExternalLinkId, reason: ReviewText) -> Self {
        Self { link, reason }
    }

    /// Returns the exact pending reservation.
    pub const fn link(&self) -> ReviewExternalLinkId {
        self.link
    }

    /// Borrows the nonempty blocking reason.
    pub const fn reason(&self) -> &ReviewText {
        &self.reason
    }
}

/// One closed exact effect committed by a terminal review pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewPassResult {
    /// Complete canonical finding inventory produced by read-only review.
    ProducedFindings(ReviewProducedFindings),
    /// One exact append-only finding event.
    FindingEvent(ReviewFindingEventResult),
    /// One exact external-object attachment.
    ExternalLinkAttachment(ReviewExternalLinkAttachmentResult),
    /// One exact external-state observation.
    ExternalLinkObservation(ReviewExternalLinkObservationResult),
    /// One unchanged external-state report that consumed its import pass.
    ExternalLinkNoChange(ReviewExternalLinkNoChangeResult),
    /// One blocked publication bound to its exact pending reservation.
    ExternalLinkPublicationBlocked(ReviewExternalLinkPublicationBlockedResult),
}

/// Canonical referenced-finding facts frozen when a reference event is admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewReferencedFindingEvidence {
    reference: ReviewFindingRef,
    status: ReviewFindingStatus,
    producer_policy: ReviewPolicy,
}

impl ReviewReferencedFindingEvidence {
    /// Freezes canonical reference, eligible status, and authenticated producer
    /// policy from a complete finding aggregate.
    pub fn try_from_finding(finding: &ReviewFinding) -> Option<Self> {
        referenced_finding_status_is_eligible(finding.status).then_some(Self {
            reference: finding.proposal.reference,
            status: finding.status,
            producer_policy: finding.proposal.producing_pass.policy(),
        })
    }

    /// Reconstitutes eligible admission evidence after authenticating the
    /// referenced finding's complete ancestry against its canonical producing
    /// pass, sealed inventory, and owning run.
    pub fn try_reconstitute(
        reference: ReviewFindingRef,
        status: ReviewFindingStatus,
        producing_pass: &ReviewPassEvidence,
        producing_run: ReviewRunEvidence,
    ) -> Option<Self> {
        if !referenced_finding_status_is_eligible(status)
            || !finding_producer_authenticates(reference, producing_pass, producing_run)
        {
            return None;
        }
        Some(Self {
            reference,
            status,
            producer_policy: producing_pass.policy(),
        })
    }

    /// Returns the complete referenced-finding identity.
    pub const fn reference(self) -> ReviewFindingRef {
        self.reference
    }

    /// Returns the status frozen at reference admission.
    pub const fn status(self) -> ReviewFindingStatus {
        self.status
    }

    /// Returns the complete frozen policy authenticated by the referenced
    /// finding's producing run.
    pub const fn producer_policy(self) -> ReviewPolicy {
        self.producer_policy
    }

    /// Returns the referenced finding's exact authenticated producing pass.
    pub const fn producing_pass(self) -> ReviewPassRef {
        self.reference.pass()
    }
}

const fn referenced_finding_status_is_eligible(status: ReviewFindingStatus) -> bool {
    matches!(
        status,
        ReviewFindingStatus::Open | ReviewFindingStatus::Accepted
    )
}

fn finding_producer_authenticates(
    reference: ReviewFindingRef,
    producing_pass: &ReviewPassEvidence,
    producing_run: ReviewRunEvidence,
) -> bool {
    producing_pass.reference() == reference.pass()
        && producing_run.reference() == reference.run()
        && producing_run.workflow() == ReviewWorkflowKind::ReadOnlyReview
        && producing_pass.kind() == ReviewPassKind::ReadOnlyReview
        && run_evidence_matches_pass(producing_run, producing_pass)
        && matches!(
            producing_pass.state(),
            ReviewPassState::Succeeded {
                result: Some(ReviewPassResult::ProducedFindings(findings)),
                ..
            } if findings.contains(reference)
                && findings
                    .findings()
                    .iter()
                    .all(|finding| finding.pass() == producing_pass.reference())
        )
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

/// Canonical run facts independently loaded for child and finding claims.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewRunEvidence {
    reference: ReviewRunRef,
    workflow: ReviewWorkflowKind,
    policy: ReviewPolicy,
    state: ReviewRunState,
}

impl ReviewRunEvidence {
    /// Supplies one independently stored run reference, workflow, policy, and
    /// lifecycle projection.
    pub const fn new(
        reference: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        policy: ReviewPolicy,
        state: ReviewRunState,
    ) -> Self {
        Self {
            reference,
            workflow,
            policy,
            state,
        }
    }

    /// Returns the canonical run reference.
    pub const fn reference(self) -> ReviewRunRef {
        self.reference
    }

    /// Returns the canonical workflow.
    pub const fn workflow(self) -> ReviewWorkflowKind {
        self.workflow
    }

    /// Returns the complete frozen policy.
    pub const fn policy(self) -> ReviewPolicy {
        self.policy
    }

    /// Returns the canonical run lifecycle projection.
    pub const fn state(self) -> ReviewRunState {
        self.state
    }
}

/// Canonical pass facts independently loaded for review claims and projections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassEvidence {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    policy: ReviewPolicy,
    state: ReviewPassState,
}

impl ReviewPassEvidence {
    const fn new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        policy: ReviewPolicy,
        state: ReviewPassState,
    ) -> Self {
        Self {
            reference,
            kind,
            policy,
            state,
        }
    }

    /// Derives authenticated evidence from one fully validated pass and its
    /// canonical run policy.
    pub fn from_pass(pass: &ReviewPass, policy: ReviewPolicy) -> Self {
        Self::new(pass.reference, pass.kind, policy, pass.state.clone())
    }

    /// Projects one exact effect result from already authenticated terminal
    /// pass evidence while preserving its identity, purpose, policy, turn, and
    /// terminal outcome.
    ///
    /// The effect aggregate remains responsible for validating that the
    /// proposed result belongs to that effect. Persistence may use this
    /// projection while atomically deciding which exact result first binds an
    /// otherwise result-free terminal pass.
    pub fn project_result(&self, result: ReviewPassResult) -> Option<Self> {
        if matches!(
            &result,
            ReviewPassResult::FindingEvent(event)
                if referenced_finding_result(event)
                    .is_some_and(|referenced| referenced.producer_policy() != self.policy)
        ) {
            return None;
        }
        let state = match &self.state {
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                result: None,
            } => ReviewPassState::Succeeded {
                turn: *turn,
                output_frontier: *output_frontier,
                result: Some(result),
            },
            ReviewPassState::Blocked { turn, result: None } => ReviewPassState::Blocked {
                turn: *turn,
                result: Some(result),
            },
            ReviewPassState::Succeeded {
                result: Some(current),
                ..
            }
            | ReviewPassState::Blocked {
                result: Some(current),
                ..
            } if current == &result => return Some(self.clone()),
            ReviewPassState::Queued
            | ReviewPassState::Running { .. }
            | ReviewPassState::Failed { .. }
            | ReviewPassState::Cancelled { .. }
            | ReviewPassState::Succeeded { .. }
            | ReviewPassState::Blocked { .. } => return None,
        };
        Some(Self::new(self.reference, self.kind, self.policy, state))
    }

    /// Returns the canonical pass reference.
    pub const fn reference(&self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the canonical pass kind.
    pub const fn kind(&self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the complete policy frozen by the pass's run.
    pub const fn policy(&self) -> ReviewPolicy {
        self.policy
    }

    /// Returns the canonical pass state.
    pub const fn state(&self) -> &ReviewPassState {
        &self.state
    }
}

/// Complete independently stored facts for review-run reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
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

    /// Returns the stored run state.
    pub const fn state(&self) -> ReviewRunState {
        self.state
    }

    /// Returns canonical pass evidence, when the run names a pass.
    pub const fn pass_evidence(&self) -> Option<&ReviewPassEvidence> {
        self.pass_evidence.as_ref()
    }
}

/// One review workflow execution against a frozen target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRun {
    reference: ReviewRunRef,
    workflow: ReviewWorkflowKind,
    policy: ReviewPolicy,
    state: ReviewRunState,
    recorded_pass: Option<ReviewPassRef>,
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
            recorded_pass: None,
        }
    }

    /// Reconstitutes a run after validating its canonical pass evidence.
    pub fn try_reconstitute(
        input: ReviewRunReconstitutionInput,
    ) -> Result<Self, ReviewRunReconstitutionError> {
        validate_run_state(
            input.reference,
            input.workflow,
            input.policy,
            input.state,
            input.pass_evidence.as_ref(),
        )
        .map_err(|failure| ReviewRunReconstitutionError {
            input: Box::new(input.clone()),
            failure,
        })?;
        Ok(Self {
            reference: input.reference,
            workflow: input.workflow,
            policy: input.policy,
            state: input.state,
            recorded_pass: input
                .pass_evidence
                .as_ref()
                .map(ReviewPassEvidence::reference),
        })
    }

    /// Applies one permitted state transition authenticated by canonical pass
    /// evidence.
    pub fn transition(
        mut self,
        next: ReviewRunState,
        pass_evidence: Option<ReviewPassEvidence>,
    ) -> Result<Self, ReviewRunTransitionError> {
        validate_run_state(
            self.reference,
            self.workflow,
            self.policy,
            next,
            pass_evidence.as_ref(),
        )
        .map_err(|failure| ReviewRunTransitionError {
            attempt: Box::new(ReviewRunTransitionAttempt {
                current: Box::new(self.clone()),
                next,
                pass_evidence: pass_evidence.clone(),
            }),
            failure: ReviewRunTransitionFailure::Evidence(failure),
        })?;
        let next_recorded_pass = pass_evidence.as_ref().map(ReviewPassEvidence::reference);
        if self.recorded_pass != next_recorded_pass {
            return Err(ReviewRunTransitionError {
                attempt: Box::new(ReviewRunTransitionAttempt {
                    current: Box::new(self.clone()),
                    next,
                    pass_evidence: pass_evidence.clone(),
                }),
                failure: ReviewRunTransitionFailure::Evidence(
                    ReviewRunEvidenceFailure::PassMismatch,
                ),
            });
        }
        let permitted = match (self.state, next) {
            (ReviewRunState::Queued, ReviewRunState::Running { .. }) => true,
            (ReviewRunState::Queued, ReviewRunState::Cancelled { last_pass: None }) => true,
            (ReviewRunState::Queued, ReviewRunState::Cancelled { last_pass: Some(_) }) => {
                pass_evidence.as_ref().is_some_and(|evidence| {
                    matches!(evidence.state(), ReviewPassState::Cancelled { turn: None })
                })
            }
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
            ) => {
                current == next
                    && pass_evidence.as_ref().is_some_and(|evidence| {
                        matches!(
                            evidence.state(),
                            ReviewPassState::Cancelled { turn: Some(_) }
                        )
                    })
            }
            _ => false,
        };
        if !permitted {
            return Err(ReviewRunTransitionError {
                attempt: Box::new(ReviewRunTransitionAttempt {
                    current: Box::new(self),
                    next,
                    pass_evidence,
                }),
                failure: ReviewRunTransitionFailure::InvalidTransition,
            });
        }
        self.state = next;
        self.recorded_pass = next_recorded_pass;
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

    /// Returns the pass already recorded for this run, when one exists.
    pub const fn recorded_pass(&self) -> Option<ReviewPassRef> {
        self.recorded_pass
    }

    /// Returns canonical identity, workflow, policy, and lifecycle evidence.
    pub const fn evidence(&self) -> ReviewRunEvidence {
        ReviewRunEvidence::new(self.reference, self.workflow, self.policy, self.state)
    }
}

fn validate_run_state(
    reference: ReviewRunRef,
    workflow: ReviewWorkflowKind,
    policy: ReviewPolicy,
    state: ReviewRunState,
    pass_evidence: Option<&ReviewPassEvidence>,
) -> Result<(), ReviewRunEvidenceFailure> {
    if let Some(pass) = state.pass()
        && pass.run() != reference
    {
        return Err(ReviewRunEvidenceFailure::ForeignPass);
    }
    if let Some(evidence) = pass_evidence {
        if evidence.reference.run() != reference {
            return Err(ReviewRunEvidenceFailure::ForeignPass);
        }
        if !workflow_matches_pass_kind(workflow, evidence.kind) {
            return Err(ReviewRunEvidenceFailure::PassKindMismatch);
        }
        if evidence.policy != policy {
            return Err(ReviewRunEvidenceFailure::PassPolicyMismatch);
        }
    }
    match state {
        ReviewRunState::Queued => match pass_evidence {
            None => Ok(()),
            Some(evidence) if evidence.state == ReviewPassState::Queued => Ok(()),
            Some(_) => Err(ReviewRunEvidenceFailure::PassStateMismatch),
        },
        ReviewRunState::Cancelled { last_pass: None } => {
            if pass_evidence.is_some() {
                Err(ReviewRunEvidenceFailure::UnexpectedPassEvidence)
            } else {
                Ok(())
            }
        }
        _ => {
            let Some(pass) = state.pass() else {
                return Err(ReviewRunEvidenceFailure::MissingPassEvidence);
            };
            let Some(evidence) = pass_evidence else {
                return Err(ReviewRunEvidenceFailure::MissingPassEvidence);
            };
            if evidence.reference != pass {
                return Err(ReviewRunEvidenceFailure::PassMismatch);
            }
            if !run_state_matches_pass(state, &evidence.state) {
                return Err(ReviewRunEvidenceFailure::PassStateMismatch);
            }
            Ok(())
        }
    }
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

fn run_state_matches_pass(run: ReviewRunState, pass: &ReviewPassState) -> bool {
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
            ReviewPassState::Cancelled { .. }
        )
    )
}

fn run_evidence_matches_pass(run: ReviewRunEvidence, pass: &ReviewPassEvidence) -> bool {
    run.reference() == pass.reference().run()
        && run.state().pass() == Some(pass.reference())
        && workflow_matches_pass_kind(run.workflow(), pass.kind())
        && run.policy() == pass.policy()
        && run_state_matches_pass(run.state(), pass.state())
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
    /// The canonical pass evidence carries another policy than the run.
    PassPolicyMismatch,
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewRunTransitionAttempt {
    current: Box<ReviewRun>,
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
        (self.attempt.current.state, self.attempt.next)
    }

    /// Returns the rejected canonical pass evidence.
    pub const fn pass_evidence(&self) -> Option<&ReviewPassEvidence> {
        self.attempt.pass_evidence.as_ref()
    }

    /// Borrows the unchanged run rejected by the transition.
    pub const fn current(&self) -> &ReviewRun {
        &self.attempt.current
    }

    /// Returns the unchanged run rejected by the transition.
    pub fn into_current(self) -> ReviewRun {
        *self.attempt.current
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
#[derive(Clone, Debug, Eq, PartialEq)]
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
        /// Exact typed effect produced by this pass, when applicable.
        result: Option<ReviewPassResult>,
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
        /// Exact blocked effect produced by this pass, when applicable.
        result: Option<ReviewPassResult>,
    },
    /// The pass was cancelled before or during its turn.
    Cancelled {
        /// Exact pass turn when activation occurred.
        turn: Option<TurnId>,
    },
}

impl ReviewPassState {
    fn turn(&self) -> Option<TurnId> {
        match self {
            Self::Queued | Self::Cancelled { turn: None } => None,
            Self::Running { turn }
            | Self::Succeeded { turn, .. }
            | Self::Failed { turn }
            | Self::Blocked { turn, .. }
            | Self::Cancelled { turn: Some(turn) } => Some(*turn),
        }
    }

    fn result(&self) -> Option<&ReviewPassResult> {
        match self {
            Self::Succeeded { result, .. } | Self::Blocked { result, .. } => result.as_ref(),
            _ => None,
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

/// Canonical accepted-input ownership and queued-origin evidence for one pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPassAcceptedInputEvidence {
    accepted_input: AcceptedInputId,
    session: SessionId,
    origin_turn: Option<TurnId>,
}

impl ReviewPassAcceptedInputEvidence {
    /// Supplies the accepted input's canonical session and optional origin turn.
    pub const fn new(
        accepted_input: AcceptedInputId,
        session: SessionId,
        origin_turn: Option<TurnId>,
    ) -> Self {
        Self {
            accepted_input,
            session,
            origin_turn,
        }
    }

    /// Returns the canonical accepted-input identity.
    pub const fn accepted_input(self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the canonical accepted-input session.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the canonical queued origin turn, when one exists.
    pub const fn origin_turn(self) -> Option<TurnId> {
        self.origin_turn
    }
}

/// Complete independently stored facts for review-pass reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassReconstitutionInput {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    workflow_run: ReviewRunRef,
    workflow: ReviewWorkflowKind,
    session: SessionId,
    accepted_input: AcceptedInputId,
    accepted_input_evidence: ReviewPassAcceptedInputEvidence,
    state: ReviewPassState,
    turn_evidence: Option<ReviewPassTurnEvidence>,
}

impl ReviewPassReconstitutionInput {
    /// Supplies the pass row plus canonical accepted-input and turn evidence.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        workflow_run: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        session: SessionId,
        accepted_input: AcceptedInputId,
        accepted_input_evidence: ReviewPassAcceptedInputEvidence,
        state: ReviewPassState,
        turn_evidence: Option<ReviewPassTurnEvidence>,
    ) -> Self {
        Self {
            reference,
            kind,
            workflow_run,
            workflow,
            session,
            accepted_input,
            accepted_input_evidence,
            state,
            turn_evidence,
        }
    }

    /// Returns the complete pass reference.
    pub const fn reference(&self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the pass kind.
    pub const fn kind(&self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the run whose canonical row supplied the workflow.
    pub const fn workflow_run(&self) -> ReviewRunRef {
        self.workflow_run
    }

    /// Returns the canonical parent run workflow.
    pub const fn workflow(&self) -> ReviewWorkflowKind {
        self.workflow
    }

    /// Returns the session stored on the pass.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the accepted input stored on the pass.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the canonical evidence loaded for the stored accepted input.
    pub const fn accepted_input_evidence(&self) -> ReviewPassAcceptedInputEvidence {
        self.accepted_input_evidence
    }

    /// Returns the stored pass state.
    pub const fn state(&self) -> &ReviewPassState {
        &self.state
    }

    /// Returns the canonical turn evidence, when the pass names a turn.
    pub const fn turn_evidence(&self) -> Option<ReviewPassTurnEvidence> {
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
    origin_turn: TurnId,
    state: ReviewPassState,
}

impl ReviewPass {
    /// Constructs a queued pass after its orchestration input is accepted.
    pub fn try_new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        run: &mut ReviewRun,
        session: SessionId,
        accepted_input: ReviewPassAcceptedInputEvidence,
    ) -> Result<Self, ReviewPassConstructionError> {
        let run_evidence = run.evidence();
        let failure = if reference.run() != run.reference {
            Some(ReviewPassConstructionFailure::ForeignRun)
        } else if !workflow_matches_pass_kind(run.workflow, kind) {
            Some(ReviewPassConstructionFailure::RunWorkflowMismatch)
        } else if run.state != ReviewRunState::Queued {
            Some(ReviewPassConstructionFailure::RunNotQueued)
        } else if run.recorded_pass.is_some() {
            Some(ReviewPassConstructionFailure::RunAlreadyHasPass)
        } else if session != accepted_input.session {
            Some(ReviewPassConstructionFailure::AcceptedInputSessionMismatch)
        } else if accepted_input.origin_turn.is_none() {
            Some(ReviewPassConstructionFailure::AcceptedInputHasNoOriginTurn)
        } else {
            None
        };
        if let Some(failure) = failure {
            return Err(ReviewPassConstructionError {
                reference,
                kind,
                run_evidence: Box::new(run_evidence),
                session,
                accepted_input: Box::new(accepted_input),
                failure,
            });
        }
        let Some(origin_turn) = accepted_input.origin_turn else {
            return Err(ReviewPassConstructionError {
                reference,
                kind,
                run_evidence: Box::new(run_evidence),
                session,
                accepted_input: Box::new(accepted_input),
                failure: ReviewPassConstructionFailure::AcceptedInputHasNoOriginTurn,
            });
        };
        run.recorded_pass = Some(reference);
        Ok(Self {
            reference,
            kind,
            session,
            accepted_input: accepted_input.accepted_input,
            origin_turn,
            state: ReviewPassState::Queued,
        })
    }

    /// Reconstitutes a pass after validating independently stored evidence.
    pub fn try_reconstitute(
        input: ReviewPassReconstitutionInput,
    ) -> Result<Self, ReviewPassReconstitutionError> {
        let failure = validate_pass_reconstitution(&input);
        if let Some(failure) = failure {
            return Err(ReviewPassReconstitutionError {
                input: Box::new(input),
                failure,
            });
        }
        let Some(origin_turn) = input.accepted_input_evidence.origin_turn else {
            return Err(ReviewPassReconstitutionError {
                input: Box::new(input),
                failure: ReviewPassReconstitutionFailure::AcceptedInputHasNoOriginTurn,
            });
        };
        Ok(Self {
            reference: input.reference,
            kind: input.kind,
            session: input.session,
            accepted_input: input.accepted_input,
            origin_turn,
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
        let same_turn = match (self.state.turn(), next.turn()) {
            (Some(current), Some(next)) => current == next,
            _ => true,
        };
        if !same_turn {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next,
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::TurnChanged,
            });
        }
        if let Some(failure) = validate_pass_turn_evidence(
            self.session,
            self.accepted_input,
            self.origin_turn,
            &next,
            turn_evidence,
        ) {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next: next.clone(),
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::Evidence(failure),
            });
        }
        if self.state == ReviewPassState::Queued
            && matches!(next, ReviewPassState::Running { .. })
            && turn_evidence
                .is_some_and(|evidence| evidence.outcome != ReviewPassTurnOutcome::Active)
        {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next: next.clone(),
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::TurnNotActive,
            });
        }
        let permitted = match (&self.state, &next) {
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
                    current: Box::new(self.clone()),
                    next: next.clone(),
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::InvalidTransition,
            });
        }
        if read_only_success_result_is_absent(self.kind, &next)
            || validate_pass_result(self.reference, self.kind, &next).is_some()
        {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next,
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::IncompatibleResult,
            });
        }
        self.state = next;
        Ok(self)
    }

    /// Monotonically binds one exact typed result to a terminal pass.
    pub fn bind_result(
        mut self,
        result: ReviewPassResult,
    ) -> Result<Self, ReviewPassTransitionError> {
        if self.state.result().is_some_and(|bound| bound == &result) {
            return Ok(self);
        }
        let next = match &self.state {
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                result: None,
            } => ReviewPassState::Succeeded {
                turn: *turn,
                output_frontier: *output_frontier,
                result: Some(result),
            },
            ReviewPassState::Blocked { turn, result: None } => ReviewPassState::Blocked {
                turn: *turn,
                result: Some(result),
            },
            state if state.result().is_some() => {
                return Err(ReviewPassTransitionError {
                    attempt: Box::new(ReviewPassTransitionAttempt {
                        current: Box::new(self.clone()),
                        next: self.state.clone(),
                        turn_evidence: None,
                    }),
                    failure: ReviewPassTransitionFailure::ResultAlreadyBound,
                });
            }
            _ => {
                return Err(ReviewPassTransitionError {
                    attempt: Box::new(ReviewPassTransitionAttempt {
                        current: Box::new(self.clone()),
                        next: self.state.clone(),
                        turn_evidence: None,
                    }),
                    failure: ReviewPassTransitionFailure::IncompatibleResult,
                });
            }
        };
        if validate_pass_result(self.reference, self.kind, &next).is_some() {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next,
                    turn_evidence: None,
                }),
                failure: ReviewPassTransitionFailure::IncompatibleResult,
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

    /// Returns the accepted input's exact canonical origin turn.
    pub const fn origin_turn(&self) -> TurnId {
        self.origin_turn
    }

    /// Returns the current pass state.
    pub const fn state(&self) -> &ReviewPassState {
        &self.state
    }
}

fn validate_pass_reconstitution(
    input: &ReviewPassReconstitutionInput,
) -> Option<ReviewPassReconstitutionFailure> {
    if input.workflow_run != input.reference.run() {
        return Some(ReviewPassReconstitutionFailure::ForeignWorkflowRun);
    }
    if !workflow_matches_pass_kind(input.workflow, input.kind) {
        return Some(ReviewPassReconstitutionFailure::RunWorkflowMismatch);
    }
    if input.accepted_input != input.accepted_input_evidence.accepted_input {
        return Some(ReviewPassReconstitutionFailure::AcceptedInputEvidenceMismatch);
    }
    if input.session != input.accepted_input_evidence.session {
        return Some(ReviewPassReconstitutionFailure::AcceptedInputSessionMismatch);
    }
    let Some(origin_turn) = input.accepted_input_evidence.origin_turn else {
        return Some(ReviewPassReconstitutionFailure::AcceptedInputHasNoOriginTurn);
    };
    if let Some(failure) = validate_pass_result(input.reference, input.kind, &input.state) {
        return Some(failure);
    }
    if let Some(failure) = validate_pass_turn_evidence(
        input.session,
        input.accepted_input,
        origin_turn,
        &input.state,
        input.turn_evidence,
    ) {
        return Some(failure);
    }
    read_only_success_result_is_absent(input.kind, &input.state)
        .then_some(ReviewPassReconstitutionFailure::IncompatibleResult)
}

fn validate_pass_turn_evidence(
    session: SessionId,
    accepted_input: AcceptedInputId,
    origin_turn: TurnId,
    state: &ReviewPassState,
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
    if evidence.turn != origin_turn {
        return Some(ReviewPassReconstitutionFailure::TurnOriginMismatch);
    }
    if evidence.session != session {
        return Some(ReviewPassReconstitutionFailure::TurnSessionMismatch);
    }
    if evidence.accepted_input != accepted_input {
        return Some(ReviewPassReconstitutionFailure::TurnAcceptedInputMismatch);
    }
    let frontier_matches_outcome = match evidence.outcome {
        ReviewPassTurnOutcome::Active => evidence.terminal_frontier.is_none(),
        _ => evidence.terminal_frontier.is_some(),
    };
    if !frontier_matches_outcome {
        return Some(ReviewPassReconstitutionFailure::TurnFrontierShapeMismatch);
    }
    if !pass_state_matches_turn_outcome(state, evidence.outcome) {
        return Some(ReviewPassReconstitutionFailure::TurnOutcomeMismatch);
    }
    if let ReviewPassState::Succeeded {
        output_frontier, ..
    } = state
        && evidence.terminal_frontier != Some(*output_frontier)
    {
        return Some(ReviewPassReconstitutionFailure::OutputFrontierMismatch);
    }
    None
}

fn pass_state_matches_turn_outcome(
    state: &ReviewPassState,
    outcome: ReviewPassTurnOutcome,
) -> bool {
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
            ReviewPassTurnOutcome::Completed
                | ReviewPassTurnOutcome::Failed
                | ReviewPassTurnOutcome::Refused
        ) | (
            ReviewPassState::Blocked { .. },
            ReviewPassTurnOutcome::ReconciliationRequired
        ) | (
            ReviewPassState::Cancelled { turn: Some(_) },
            ReviewPassTurnOutcome::Cancelled
        )
    )
}
fn read_only_success_result_is_absent(kind: ReviewPassKind, state: &ReviewPassState) -> bool {
    kind == ReviewPassKind::ReadOnlyReview
        && matches!(state, ReviewPassState::Succeeded { result: None, .. })
}

fn validate_pass_result(
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    state: &ReviewPassState,
) -> Option<ReviewPassReconstitutionFailure> {
    let Some(result) = state.result() else {
        return (kind == ReviewPassKind::Publish
            && matches!(state, ReviewPassState::Blocked { .. }))
        .then_some(ReviewPassReconstitutionFailure::IncompatibleResult);
    };
    let foreign_target = match result {
        ReviewPassResult::ProducedFindings(findings) => findings
            .findings()
            .iter()
            .any(|finding| finding.target() != reference.target()),
        ReviewPassResult::FindingEvent(event) => {
            event.finding().target() != reference.target()
                || referenced_finding_result(event)
                    .is_some_and(|referenced| referenced.reference().target() != reference.target())
        }
        ReviewPassResult::ExternalLinkAttachment(result) => result
            .finding_event()
            .is_some_and(|event| event.finding().target() != reference.target()),
        ReviewPassResult::ExternalLinkObservation(_)
        | ReviewPassResult::ExternalLinkNoChange(_)
        | ReviewPassResult::ExternalLinkPublicationBlocked(_) => false,
    };
    if foreign_target {
        return Some(ReviewPassReconstitutionFailure::ForeignResultTarget);
    }
    let compatible = match (state, result) {
        (ReviewPassState::Succeeded { .. }, ReviewPassResult::ProducedFindings(findings))
            if kind == ReviewPassKind::ReadOnlyReview =>
        {
            findings
                .findings()
                .iter()
                .all(|finding| finding.pass() == reference)
        }
        (ReviewPassState::Succeeded { .. }, ReviewPassResult::FindingEvent(event)) => {
            !matches!(event.kind(), ReviewFindingEventResultKind::Posted { .. })
                && event.finding().target() == reference.target()
                && finding_event_result_reference_is_compatible(event)
                && finding_event_result_matches_pass(
                    event.kind(),
                    kind,
                    ReviewPassTurnOutcome::Completed,
                )
        }
        (ReviewPassState::Blocked { .. }, ReviewPassResult::FindingEvent(event)) => {
            event.finding().target() == reference.target()
                && finding_event_result_reference_is_compatible(event)
                && finding_event_result_matches_pass(
                    event.kind(),
                    kind,
                    ReviewPassTurnOutcome::ReconciliationRequired,
                )
        }
        (ReviewPassState::Succeeded { .. }, ReviewPassResult::ExternalLinkAttachment(result)) => {
            matches!(
                kind,
                ReviewPassKind::Publish | ReviewPassKind::ImportExternalContext
            ) && result.finding_event().is_none_or(|event| {
                matches!(
                    event.kind(),
                    ReviewFindingEventResultKind::Posted { link }
                        if *link == result.link()
                ) && finding_event_result_matches_pass(
                    event.kind(),
                    kind,
                    ReviewPassTurnOutcome::Completed,
                )
            })
        }
        (ReviewPassState::Succeeded { .. }, ReviewPassResult::ExternalLinkObservation(_)) => {
            kind == ReviewPassKind::ImportExternalContext
        }
        (ReviewPassState::Succeeded { .. }, ReviewPassResult::ExternalLinkNoChange(_)) => {
            kind == ReviewPassKind::ImportExternalContext
        }
        (ReviewPassState::Blocked { .. }, ReviewPassResult::ExternalLinkPublicationBlocked(_)) => {
            kind == ReviewPassKind::Publish
        }
        _ => false,
    };
    (!compatible).then_some(ReviewPassReconstitutionFailure::IncompatibleResult)
}

fn referenced_finding_result(
    event: &ReviewFindingEventResult,
) -> Option<ReviewReferencedFindingEvidence> {
    match event.kind() {
        ReviewFindingEventResultKind::Duplicate { canonical } => Some(*canonical),
        ReviewFindingEventResultKind::Superseded { successor } => Some(*successor),
        _ => None,
    }
}

fn finding_event_result_reference_is_compatible(event: &ReviewFindingEventResult) -> bool {
    referenced_finding_result(event).is_none_or(|referenced| {
        referenced.reference().finding() != event.finding().finding()
            && referenced.reference().target() == event.finding().target()
            && referenced_finding_status_is_eligible(referenced.status())
    })
}

fn finding_event_result_matches_pass(
    event: &ReviewFindingEventResultKind,
    pass: ReviewPassKind,
    outcome: ReviewPassTurnOutcome,
) -> bool {
    matches!(
        (event, pass, outcome),
        (
            ReviewFindingEventResultKind::Accepted
                | ReviewFindingEventResultKind::Rejected { .. }
                | ReviewFindingEventResultKind::Stale,
            ReviewPassKind::Judge,
            ReviewPassTurnOutcome::Completed
        ) | (
            ReviewFindingEventResultKind::Duplicate { .. }
                | ReviewFindingEventResultKind::Superseded { .. },
            ReviewPassKind::Dedupe,
            ReviewPassTurnOutcome::Completed
        ) | (
            ReviewFindingEventResultKind::Posted { .. },
            ReviewPassKind::Publish | ReviewPassKind::ImportExternalContext,
            ReviewPassTurnOutcome::Completed
        ) | (
            ReviewFindingEventResultKind::Fixed,
            ReviewPassKind::Fix,
            ReviewPassTurnOutcome::Completed
        ) | (
            ReviewFindingEventResultKind::BlockedWithReason { link: Some(_), .. },
            ReviewPassKind::Publish,
            ReviewPassTurnOutcome::ReconciliationRequired
        ) | (
            ReviewFindingEventResultKind::BlockedWithReason { link: None, .. },
            ReviewPassKind::Fix,
            ReviewPassTurnOutcome::ReconciliationRequired
        )
    )
}

/// Why queued-pass construction was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassConstructionFailure {
    /// The supplied run evidence belongs to another run.
    ForeignRun,
    /// The pass kind is incompatible with its canonical run workflow.
    RunWorkflowMismatch,
    /// A pass can be admitted only while its run is queued.
    RunNotQueued,
    /// The run already records its one permitted pass.
    RunAlreadyHasPass,
    /// The accepted input belongs to another session.
    AcceptedInputSessionMismatch,
    /// The accepted input is not canonically classified as a queued turn origin.
    AcceptedInputHasNoOriginTurn,
}

/// Rejected queued-pass construction retaining all supplied facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassConstructionError {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    run_evidence: Box<ReviewRunEvidence>,
    session: SessionId,
    accepted_input: Box<ReviewPassAcceptedInputEvidence>,
    failure: ReviewPassConstructionFailure,
}

impl ReviewPassConstructionError {
    /// Returns the rejected pass reference.
    pub const fn reference(&self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the rejected pass kind.
    pub const fn kind(&self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the canonical parent workflow supplied for the pass.
    pub const fn workflow(&self) -> ReviewWorkflowKind {
        self.run_evidence.workflow
    }

    /// Returns the independently supplied canonical parent-run evidence.
    pub const fn run_evidence(&self) -> ReviewRunEvidence {
        *self.run_evidence
    }

    /// Returns the pass and canonical accepted-input sessions.
    pub const fn sessions(&self) -> (SessionId, SessionId) {
        (self.session, self.accepted_input.session)
    }

    /// Returns the accepted input whose association did not match.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input.accepted_input
    }

    /// Returns the claimed canonical queued-origin turn, when one exists.
    pub const fn origin_turn(&self) -> Option<TurnId> {
        self.accepted_input.origin_turn
    }

    /// Returns why construction was rejected.
    pub const fn failure(&self) -> ReviewPassConstructionFailure {
        self.failure
    }
}

/// Why stored pass facts cannot reconstruct one canonical pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassReconstitutionFailure {
    /// The workflow discriminator was loaded from another run's row.
    ForeignWorkflowRun,
    /// The pass kind is incompatible with its canonical run workflow.
    RunWorkflowMismatch,
    /// The accepted-input identity stored on the pass differs from the
    /// supplied canonical accepted-input evidence.
    AcceptedInputEvidenceMismatch,
    /// The accepted input belongs to another session.
    AcceptedInputSessionMismatch,
    /// The accepted input is pending or consumed steering with no origin turn.
    AcceptedInputHasNoOriginTurn,
    /// The pass names a turn but no canonical turn row was supplied.
    MissingTurnEvidence,
    /// A turn row was supplied for a pass state with no turn.
    UnexpectedTurnEvidence,
    /// The canonical turn identity differs from the pass state.
    TurnMismatch,
    /// The pass turn differs from the accepted input's canonical origin turn.
    TurnOriginMismatch,
    /// The canonical turn belongs to another session.
    TurnSessionMismatch,
    /// The canonical turn was originated by another accepted input.
    TurnAcceptedInputMismatch,
    /// The canonical turn lifecycle outcome differs from the pass projection.
    TurnOutcomeMismatch,
    /// The canonical turn's terminal frontier contradicts its outcome: a
    /// terminal turn always carries one and an active turn never does.
    TurnFrontierShapeMismatch,
    /// The successful output is not the canonical terminal frontier.
    OutputFrontierMismatch,
    /// The typed pass result is incompatible with its pass kind, outcome, or
    /// ownership.
    IncompatibleResult,
    /// A pass result names a finding from another target.
    ForeignResultTarget,
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
    /// A queued pass starts only while its canonical turn is active; terminal
    /// lag is reserved for a pass that already projected the running start.
    TurnNotActive,
    /// A typed result does not match this pass kind, outcome, or ownership.
    IncompatibleResult,
    /// A distinct result was supplied after the pass result became immutable.
    ResultAlreadyBound,
}

/// Rejected pass transition retaining both states.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassTransitionError {
    attempt: Box<ReviewPassTransitionAttempt>,
    failure: ReviewPassTransitionFailure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewPassTransitionAttempt {
    current: Box<ReviewPass>,
    next: ReviewPassState,
    turn_evidence: Option<ReviewPassTurnEvidence>,
}

impl ReviewPassTransitionError {
    /// Returns why the transition was rejected.
    pub const fn failure(&self) -> ReviewPassTransitionFailure {
        self.failure
    }

    /// Returns the current and requested states.
    pub fn states(&self) -> (ReviewPassState, ReviewPassState) {
        (
            self.attempt.current.state.clone(),
            self.attempt.next.clone(),
        )
    }

    /// Returns the rejected canonical turn evidence.
    pub const fn turn_evidence(&self) -> Option<ReviewPassTurnEvidence> {
        self.attempt.turn_evidence
    }

    /// Borrows the unchanged pass rejected by the transition.
    pub const fn current(&self) -> &ReviewPass {
        &self.attempt.current
    }

    /// Returns the unchanged pass rejected by the transition.
    pub fn into_current(self) -> ReviewPass {
        *self.attempt.current
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
    confidence_axes: ReviewFindingConfidenceAxes,
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
        confidence_axes: ReviewFindingConfidenceAxes,
        category: ReviewKey,
        recommended_fix: Option<ReviewText>,
    ) -> Self {
        Self {
            location,
            title,
            body,
            severity,
            confidence_axes,
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

    /// Returns producer confidence that the issue is real.
    pub const fn is_real_confidence(&self) -> ReviewConfidence {
        self.confidence_axes.is_real_confidence()
    }

    /// Returns producer confidence that the severity label is correct.
    pub const fn severity_label_confidence(&self) -> ReviewConfidence {
        self.confidence_axes.severity_label_confidence()
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
        producing_run: ReviewRunEvidence,
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
        if producing_run.reference() != reference.run() {
            return Err(ReviewFindingTransitionError::proposal(
                ReviewFindingTransitionFailure::ForeignProducingRun,
            ));
        }
        if producing_run.workflow() != ReviewWorkflowKind::ReadOnlyReview
            || !run_evidence_matches_pass(producing_run, &producing_pass)
        {
            return Err(ReviewFindingTransitionError::proposal(
                ReviewFindingTransitionFailure::IncompatibleProducingRunEvidence,
            ));
        }
        if producing_pass.kind() != ReviewPassKind::ReadOnlyReview
            || !matches!(
                producing_pass.state(),
                ReviewPassState::Succeeded {
                    result: Some(ReviewPassResult::ProducedFindings(findings)),
                    ..
                } if findings.contains(reference)
                    && findings
                        .findings()
                        .iter()
                        .all(|finding| finding.pass() == producing_pass.reference())
            )
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
    pub const fn producing_pass(&self) -> &ReviewPassEvidence {
        &self.producing_pass
    }

    /// Borrows immutable finding content.
    pub const fn content(&self) -> &ReviewFindingContent {
        &self.content
    }
}

/// A finding-associated external-link reference.
#[derive(Clone, Debug, Eq, PartialEq)]
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
        if !matches!(
            link.object_kind(),
            ReviewExternalObjectKind::Review
                | ReviewExternalObjectKind::ReviewThread
                | ReviewExternalObjectKind::ReviewComment
                | ReviewExternalObjectKind::ChangeRequestComment
        ) {
            return Err(ReviewFindingExternalLinkError {
                finding,
                link: link.id(),
                association: link.association(),
                failure: ReviewFindingExternalLinkFailure::IncompatibleObjectKind,
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
            attachment_pass: attachment.pass_evidence().clone(),
        })
    }

    /// Returns the finding reference.
    pub const fn finding(&self) -> ReviewFindingRef {
        self.finding
    }

    /// Returns the external-link identity.
    pub const fn link(&self) -> ReviewExternalLinkId {
        self.link
    }

    /// Returns the canonical pass that produced the attachment.
    pub const fn attachment_pass(&self) -> &ReviewPassEvidence {
        &self.attachment_pass
    }
}

/// Why a canonical external link cannot support a posted finding event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewFindingExternalLinkFailure {
    /// The link is not canonically associated with the finding.
    ForeignAssociation,
    /// The external object does not carry review content.
    IncompatibleObjectKind,
    /// The link remains a pending reservation without an external identity.
    NotAttached,
    /// The link is already attached and cannot prove a pending publication.
    AlreadyAttached,
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

/// A finding-associated pending publication reservation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFindingPendingExternalLinkRef {
    finding: ReviewFindingRef,
    link: ReviewExternalLinkId,
}

impl ReviewFindingPendingExternalLinkRef {
    /// Derives a finding-bound reference from one pending canonical link.
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
        if !matches!(
            link.object_kind(),
            ReviewExternalObjectKind::Review
                | ReviewExternalObjectKind::ReviewThread
                | ReviewExternalObjectKind::ReviewComment
                | ReviewExternalObjectKind::ChangeRequestComment
        ) {
            return Err(ReviewFindingExternalLinkError {
                finding,
                link: link.id(),
                association: link.association(),
                failure: ReviewFindingExternalLinkFailure::IncompatibleObjectKind,
            });
        }
        if link.attachment().is_some() {
            return Err(ReviewFindingExternalLinkError {
                finding,
                link: link.id(),
                association: link.association(),
                failure: ReviewFindingExternalLinkFailure::AlreadyAttached,
            });
        }
        Ok(Self {
            finding,
            link: link.id(),
        })
    }

    /// Returns the finding reference.
    pub const fn finding(&self) -> ReviewFindingRef {
        self.finding
    }

    /// Returns the pending external-link identity.
    pub const fn link(&self) -> ReviewExternalLinkId {
        self.link
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
        /// Canonical finding and its authenticated status at admission.
        canonical: ReviewReferencedFindingEvidence,
    },
    /// A later finding superseded this finding.
    Superseded {
        /// Successor finding and its authenticated status at admission.
        successor: ReviewReferencedFindingEvidence,
    },
    /// The finding no longer applies to the target.
    Stale,
    /// The finding was published through the exact external link.
    Posted {
        /// Finding-bound external-link reference.
        link: Box<ReviewFindingExternalLinkRef>,
    },
    /// A repair pass fixed the finding.
    Fixed,
    /// A repair or publication pass could not proceed.
    BlockedWithReason {
        /// Exact nonempty reason.
        reason: ReviewText,
        /// Pending publication reservation; absent only for a repair block.
        link: Option<Box<ReviewFindingPendingExternalLinkRef>>,
    },
}

impl ReviewFindingEventKind {
    /// Returns the closed discriminator committed by the producing pass.
    pub const fn event_type(&self) -> ReviewFindingEventType {
        match self {
            Self::Accepted => ReviewFindingEventType::Accepted,
            Self::Rejected { .. } => ReviewFindingEventType::Rejected,
            Self::Duplicate { .. } => ReviewFindingEventType::Duplicate,
            Self::Superseded { .. } => ReviewFindingEventType::Superseded,
            Self::Stale => ReviewFindingEventType::Stale,
            Self::Posted { .. } => ReviewFindingEventType::Posted,
            Self::Fixed => ReviewFindingEventType::Fixed,
            Self::BlockedWithReason { .. } => ReviewFindingEventType::BlockedWithReason,
        }
    }

    fn result_kind(&self) -> ReviewFindingEventResultKind {
        match self {
            Self::Accepted => ReviewFindingEventResultKind::Accepted,
            Self::Rejected { reason } => ReviewFindingEventResultKind::Rejected {
                reason: reason.clone(),
            },
            Self::Duplicate { canonical } => ReviewFindingEventResultKind::Duplicate {
                canonical: *canonical,
            },
            Self::Superseded { successor } => ReviewFindingEventResultKind::Superseded {
                successor: *successor,
            },
            Self::Stale => ReviewFindingEventResultKind::Stale,
            Self::Posted { link } => ReviewFindingEventResultKind::Posted { link: link.link() },
            Self::Fixed => ReviewFindingEventResultKind::Fixed,
            Self::BlockedWithReason { reason, link } => {
                ReviewFindingEventResultKind::BlockedWithReason {
                    reason: reason.clone(),
                    link: link.as_ref().map(|link| link.link()),
                }
            }
        }
    }
}

/// One append-only finding lifecycle record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFindingEvent {
    finding: ReviewFindingRef,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassRef,
    pass_evidence: ReviewPassEvidence,
    run: ReviewRunEvidence,
    kind: ReviewFindingEventKind,
}

impl ReviewFindingEvent {
    /// Constructs one typed event.
    pub const fn new(
        finding: ReviewFindingRef,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassRef,
        pass_evidence: ReviewPassEvidence,
        run: ReviewRunEvidence,
        kind: ReviewFindingEventKind,
    ) -> Self {
        Self {
            finding,
            ordinal,
            pass,
            pass_evidence,
            run,
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
        self.pass
    }

    /// Returns the canonical producing-pass evidence.
    pub const fn pass_evidence(&self) -> &ReviewPassEvidence {
        &self.pass_evidence
    }

    /// Returns the canonical producing-run evidence.
    pub const fn run_evidence(&self) -> ReviewRunEvidence {
        self.run
    }

    /// Borrows the event kind and evidence.
    pub const fn kind(&self) -> &ReviewFindingEventKind {
        &self.kind
    }
}

/// One finding aggregate with complete append-only event history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFinding {
    proposal: ReviewFindingProposal,
    events: Vec<ReviewFindingEvent>,
    status: ReviewFindingStatus,
    pass_claims: BTreeMap<ReviewPassId, (ReviewPassEvidence, ReviewRunEvidence)>,
    run_claims: BTreeMap<ReviewRunId, (ReviewPassEvidence, ReviewRunEvidence)>,
    publication_links: BTreeSet<ReviewExternalLinkId>,
}

impl ReviewFinding {
    /// Constructs an open finding.
    pub const fn new(proposal: ReviewFindingProposal) -> Self {
        Self {
            proposal,
            events: Vec::new(),
            status: ReviewFindingStatus::Open,
            pass_claims: BTreeMap::new(),
            run_claims: BTreeMap::new(),
            publication_links: BTreeSet::new(),
        }
    }

    fn event_error(
        &self,
        event: ReviewFindingEvent,
        failure: ReviewFindingTransitionFailure,
    ) -> ReviewFindingTransitionError {
        ReviewFindingTransitionError {
            current: Some(Box::new(self.clone())),
            event: Some(Box::new(event)),
            failure,
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
            return Err(
                self.event_error(event, ReviewFindingTransitionFailure::ForeignEventFinding)
            );
        }
        let expected = match self.events.last() {
            Some(previous) => previous.ordinal.checked_successor(),
            None => Some(ReviewEventOrdinal::one()),
        };
        if expected != Some(event.ordinal) {
            return Err(self.event_error(
                event,
                ReviewFindingTransitionFailure::NoncontiguousOrdinal { expected },
            ));
        }
        if event.pass.target() != self.proposal.reference.target() {
            return Err(self.event_error(event, ReviewFindingTransitionFailure::ForeignEventPass));
        }
        if event.pass != event.pass_evidence.reference() {
            return Err(self.event_error(
                event,
                ReviewFindingTransitionFailure::EventPassEvidenceMismatch,
            ));
        }
        if !run_evidence_matches_pass(event.run, &event.pass_evidence) {
            return Err(self.event_error(
                event,
                ReviewFindingTransitionFailure::IncompatibleEventRunEvidence,
            ));
        }
        if event.run.policy() != event.pass_evidence.policy()
            || event.run.policy() != self.proposal.producing_pass.policy()
        {
            return Err(
                self.event_error(event, ReviewFindingTransitionFailure::EventPolicyMismatch)
            );
        }
        let producing_claim = (
            &self.proposal.producing_pass,
            ReviewRunEvidence::new(
                self.proposal.reference.run(),
                ReviewWorkflowKind::ReadOnlyReview,
                self.proposal.producing_pass.policy(),
                ReviewRunState::Succeeded {
                    concluding_pass: self.proposal.producing_pass.reference(),
                },
            ),
        );
        let prior_pass = if self.proposal.producing_pass.reference().pass() == event.pass.pass() {
            Some(producing_claim)
        } else {
            self.pass_claims
                .get(&event.pass.pass())
                .map(|(pass, run)| (pass, *run))
        };
        if prior_pass.is_some_and(|prior| prior != (&event.pass_evidence, event.run)) {
            return Err(self.event_error(
                event,
                ReviewFindingTransitionFailure::ConflictingPassEvidence,
            ));
        }
        let prior_run = if self.proposal.reference.run().run() == event.run.reference().run() {
            Some((
                &self.proposal.producing_pass,
                ReviewRunEvidence::new(
                    self.proposal.reference.run(),
                    ReviewWorkflowKind::ReadOnlyReview,
                    self.proposal.producing_pass.policy(),
                    ReviewRunState::Succeeded {
                        concluding_pass: self.proposal.producing_pass.reference(),
                    },
                ),
            ))
        } else {
            self.run_claims
                .get(&event.run.reference().run())
                .map(|(pass, run)| (pass, *run))
        };
        if prior_run.is_some_and(|prior| prior != (&event.pass_evidence, event.run)) {
            return Err(self.event_error(
                event,
                ReviewFindingTransitionFailure::ConflictingRunEvidence,
            ));
        }
        if !finding_event_matches_pass_evidence(&event, &event.pass_evidence) {
            return Err(self.event_error(
                event,
                ReviewFindingTransitionFailure::IncompatibleEventPassEvidence,
            ));
        }
        if matches!(&event.kind, ReviewFindingEventKind::Accepted)
            && self.proposal.content.is_real_confidence()
                < event.run.policy().minimum_judge_confidence()
        {
            return Err(self.event_error(
                event,
                ReviewFindingTransitionFailure::BelowJudgmentThreshold,
            ));
        }
        if matches!(&event.kind, ReviewFindingEventKind::Posted { .. })
            && self.proposal.content.is_real_confidence()
                < event.run.policy().minimum_publication_confidence()
        {
            return Err(self.event_error(
                event,
                ReviewFindingTransitionFailure::BelowPublicationThreshold,
            ));
        }
        validate_finding_reference(&self, &event)?;
        if let ReviewFindingEventKind::Posted { link } = &event.kind
            && self.publication_links.contains(&link.link())
        {
            return Err(
                self.event_error(event, ReviewFindingTransitionFailure::ReusedPublicationLink)
            );
        }
        let Some(next_status) = finding_transition(self.status, &event.kind, self.events.last())
        else {
            return Err(self.event_error(
                event,
                ReviewFindingTransitionFailure::InvalidTransition {
                    current: self.status,
                },
            ));
        };
        let pass_claim = (event.pass_evidence.clone(), event.run);
        self.pass_claims
            .entry(event.pass.pass())
            .or_insert_with(|| pass_claim.clone());
        self.run_claims
            .entry(event.run.reference().run())
            .or_insert(pass_claim);
        if let ReviewFindingEventKind::Posted { link } = &event.kind {
            self.publication_links.insert(link.link());
        }
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

/// Validates the complete finding-reference graph loaded for one immutable
/// target, rejecting missing roots and direct or transitive cycles.
pub fn validate_complete_review_finding_reference_graph(
    findings: &[ReviewFinding],
) -> Result<(), Box<ReviewFindingReferenceGraphError>> {
    let mut references = BTreeMap::new();
    let Some(expected_target) = findings
        .first()
        .map(|finding| finding.proposal.reference.target())
    else {
        return Ok(());
    };
    for finding in findings {
        let reference = finding.proposal.reference;
        if expected_target != reference.target() {
            return Err(Box::new(
                ReviewFindingReferenceGraphError::ForeignTargetRoot {
                    expected: expected_target,
                    actual: reference.target(),
                },
            ));
        }
        let referenced = finding.events.iter().find_map(|event| match &event.kind {
            ReviewFindingEventKind::Duplicate { canonical } => Some(*canonical),
            ReviewFindingEventKind::Superseded { successor } => Some(*successor),
            ReviewFindingEventKind::Accepted
            | ReviewFindingEventKind::Rejected { .. }
            | ReviewFindingEventKind::Stale
            | ReviewFindingEventKind::Posted { .. }
            | ReviewFindingEventKind::Fixed
            | ReviewFindingEventKind::BlockedWithReason { .. } => None,
        });
        let producer_policy = finding.proposal.producing_pass.policy();
        if references
            .insert(reference, (producer_policy, referenced))
            .is_some()
        {
            return Err(Box::new(
                ReviewFindingReferenceGraphError::DuplicateFinding { reference },
            ));
        }
    }
    for (&finding, (_, referenced)) in &references {
        let Some(referenced) = referenced else {
            continue;
        };
        let referenced_identity = referenced.reference();
        let Some((referenced_root_policy, _)) = references.get(&referenced_identity) else {
            return Err(Box::new(
                ReviewFindingReferenceGraphError::MissingReferencedFinding {
                    finding,
                    referenced: referenced_identity,
                },
            ));
        };
        if referenced.producer_policy() != *referenced_root_policy {
            return Err(Box::new(
                ReviewFindingReferenceGraphError::ReferencedFindingPolicyMismatch {
                    finding,
                    referenced: referenced_identity,
                },
            ));
        }
    }

    let mut completed = BTreeSet::new();
    for root in references.keys().copied() {
        let mut path = BTreeSet::new();
        let mut current = root;
        while !completed.contains(&current) {
            path.insert(current);
            let Some((_, Some(referenced))) = references.get(&current) else {
                break;
            };
            let referenced = referenced.reference();
            if path.contains(&referenced) {
                return Err(Box::new(ReviewFindingReferenceGraphError::Cycle {
                    finding: current,
                    referenced,
                }));
            }
            current = referenced;
        }
        completed.extend(path);
    }
    Ok(())
}

/// Why a complete reconstituted finding-reference graph is corrupt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewFindingReferenceGraphError {
    /// One complete finding identity appeared more than once.
    DuplicateFinding {
        /// Repeated complete finding identity.
        reference: ReviewFindingRef,
    },
    /// One supplied root belongs to another immutable target.
    ForeignTargetRoot {
        /// Target established by the first supplied root.
        expected: ReviewTargetId,
        /// Foreign target carried by the rejected root.
        actual: ReviewTargetId,
    },
    /// A reference edge names a finding absent from the complete target graph.
    MissingReferencedFinding {
        /// Finding that owns the corrupt edge.
        finding: ReviewFindingRef,
        /// Referenced finding absent from the graph.
        referenced: ReviewFindingRef,
    },
    /// A reference edge freezes a policy different from its supplied root.
    ReferencedFindingPolicyMismatch {
        /// Finding that owns the corrupt edge.
        finding: ReviewFindingRef,
        /// Referenced root whose producer policy contradicts the edge.
        referenced: ReviewFindingRef,
    },
    /// One edge closes a direct or transitive reference cycle.
    Cycle {
        /// Finding that owns the cycle-closing edge.
        finding: ReviewFindingRef,
        /// Earlier finding reached again by that edge.
        referenced: ReviewFindingRef,
    },
}

impl std::fmt::Display for ReviewFindingReferenceGraphError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateFinding { reference } => write!(
                formatter,
                "review finding reference graph repeats finding {reference:?}",
            ),
            Self::ForeignTargetRoot { expected, actual } => write!(
                formatter,
                "review finding reference graph contains target {actual:?}; expected {expected:?}",
            ),
            Self::MissingReferencedFinding {
                finding,
                referenced,
            } => write!(
                formatter,
                "review finding {finding:?} references missing finding {referenced:?}",
            ),
            Self::ReferencedFindingPolicyMismatch {
                finding,
                referenced,
            } => write!(
                formatter,
                "review finding {finding:?} freezes a policy that differs from referenced finding {referenced:?}",
            ),
            Self::Cycle {
                finding,
                referenced,
            } => write!(
                formatter,
                "review finding {finding:?} closes a reference cycle through {referenced:?}",
            ),
        }
    }
}

impl std::error::Error for ReviewFindingReferenceGraphError {}

fn validate_finding_reference(
    finding: &ReviewFinding,
    event: &ReviewFindingEvent,
) -> Result<(), ReviewFindingTransitionError> {
    let proposal = &finding.proposal;
    let referenced = match &event.kind {
        ReviewFindingEventKind::Duplicate { canonical } => Some(*canonical),
        ReviewFindingEventKind::Superseded { successor } => Some(*successor),
        ReviewFindingEventKind::Posted { link } => {
            if link.finding() != proposal.reference {
                return Err(finding.event_error(
                    event.clone(),
                    ReviewFindingTransitionFailure::ForeignExternalLink,
                ));
            }
            if link.attachment_pass() != event.pass_evidence() {
                return Err(finding.event_error(
                    event.clone(),
                    ReviewFindingTransitionFailure::PublicationPassMismatch,
                ));
            }
            None
        }
        ReviewFindingEventKind::BlockedWithReason {
            link: Some(link), ..
        } => {
            if link.finding() != proposal.reference {
                return Err(finding.event_error(
                    event.clone(),
                    ReviewFindingTransitionFailure::ForeignExternalLink,
                ));
            }
            None
        }
        _ => None,
    };
    if let Some(referenced) = referenced {
        if referenced.reference().finding() == proposal.reference.finding() {
            return Err(
                finding.event_error(event.clone(), ReviewFindingTransitionFailure::SelfReference)
            );
        }
        if referenced.reference().target() != proposal.reference.target() {
            return Err(finding.event_error(
                event.clone(),
                ReviewFindingTransitionFailure::ForeignReferencedFinding,
            ));
        }
        if referenced.producer_policy() != proposal.producing_pass.policy() {
            return Err(finding.event_error(
                event.clone(),
                ReviewFindingTransitionFailure::ReferencedFindingPolicyMismatch,
            ));
        }
        if !referenced_finding_status_is_eligible(referenced.status()) {
            return Err(finding.event_error(
                event.clone(),
                ReviewFindingTransitionFailure::IneligibleReferencedFinding,
            ));
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
        (Status::BlockedWithReason, Event::Posted { link })
            if previous.is_some_and(|event| {
                matches!(
                    event.kind(),
                    Event::BlockedWithReason {
                        link: Some(blocked_link),
                        ..
                    } if blocked_link.link() == link.link()
                )
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
    event: &ReviewFindingEvent,
    pass: &ReviewPassEvidence,
) -> bool {
    let kind_matches = matches!(
        (&event.kind, pass.kind()),
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
            ReviewPassKind::Publish | ReviewPassKind::ImportExternalContext
        ) | (ReviewFindingEventKind::Fixed, ReviewPassKind::Fix)
            | (
                ReviewFindingEventKind::BlockedWithReason { link: Some(_), .. },
                ReviewPassKind::Publish
            )
            | (
                ReviewFindingEventKind::BlockedWithReason { link: None, .. },
                ReviewPassKind::Fix
            )
    );
    let expected =
        ReviewFindingEventResult::new(event.finding, event.ordinal, event.kind.result_kind());
    let outcome_matches = if let ReviewFindingEventKind::Posted { link } = &event.kind {
        matches!(
            pass.state(),
            ReviewPassState::Succeeded {
                result: Some(ReviewPassResult::ExternalLinkAttachment(attachment)),
                ..
            } if attachment.link() == link.link()
                && attachment.finding_event() == Some(&expected)
        )
    } else if matches!(
        &event.kind,
        ReviewFindingEventKind::BlockedWithReason { .. }
    ) {
        matches!(
            pass.state(),
            ReviewPassState::Blocked {
                result: Some(ReviewPassResult::FindingEvent(actual)),
                ..
            } if actual == &expected
        )
    } else {
        matches!(
            pass.state(),
            ReviewPassState::Succeeded {
                result: Some(ReviewPassResult::FindingEvent(actual)),
                ..
            } if actual == &expected
        )
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
    /// The independently loaded producing run does not own the finding.
    ForeignProducingRun,
    /// The producing run's workflow or policy contradicts its pass evidence.
    IncompatibleProducingRunEvidence,
    /// The producing pass did not canonically succeed as read-only review.
    IncompatibleProducingPassEvidence,
    /// An event belongs to another finding.
    ForeignEventFinding,
    /// An event pass belongs to another target.
    ForeignEventPass,
    /// The pass identity stored on the event differs from the supplied
    /// canonical pass evidence.
    EventPassEvidenceMismatch,
    /// The event pass contradicts its independently loaded owning run.
    IncompatibleEventRunEvidence,
    /// An event pass's run carries a different policy from the finding.
    EventPolicyMismatch,
    /// One pass identity was supplied with contradictory canonical evidence.
    ConflictingPassEvidence,
    /// One run identity was supplied with another pass, workflow, or policy.
    ConflictingRunEvidence,
    /// The event kind or outcome cannot be produced by the canonical pass.
    IncompatibleEventPassEvidence,
    /// Accepted judgment is below the frozen judgment threshold.
    BelowJudgmentThreshold,
    /// External publication is below the frozen publication threshold.
    BelowPublicationThreshold,
    /// A duplicate or successor finding belongs to another target.
    ForeignReferencedFinding,
    /// A duplicate or successor finding was produced under another frozen
    /// policy.
    ReferencedFindingPolicyMismatch,
    /// A duplicate or successor names a finding that cannot receive a reference.
    IneligibleReferencedFinding,
    /// A finding names itself as its canonical or successor finding.
    SelfReference,
    /// A publication link belongs to another finding.
    ForeignExternalLink,
    /// A posted event names a pass other than the attachment producer.
    PublicationPassMismatch,
    /// A posted event reuses attachment evidence consumed by an earlier post.
    ReusedPublicationLink,
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
    current: Option<Box<ReviewFinding>>,
    event: Option<Box<ReviewFindingEvent>>,
    failure: ReviewFindingTransitionFailure,
}

impl ReviewFindingTransitionError {
    fn proposal(failure: ReviewFindingTransitionFailure) -> Self {
        Self {
            current: None,
            event: None,
            failure,
        }
    }

    /// Returns why construction or transition failed.
    pub const fn failure(&self) -> ReviewFindingTransitionFailure {
        self.failure
    }

    /// Borrows the unchanged finding rejected by an event transition.
    pub fn current(&self) -> Option<&ReviewFinding> {
        self.current.as_deref()
    }

    /// Borrows the rejected event, when failure occurred during event application.
    pub fn event(&self) -> Option<&ReviewFindingEvent> {
        match self.event.as_ref() {
            Some(event) => Some(event.as_ref()),
            None => None,
        }
    }

    /// Returns the optional unchanged finding, rejected event, and failure.
    pub fn into_parts(
        self,
    ) -> (
        Option<ReviewFinding>,
        Option<ReviewFindingEvent>,
        ReviewFindingTransitionFailure,
    ) {
        (
            self.current.map(|current| *current),
            self.event.map(|event| *event),
            self.failure,
        )
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
    pass: ReviewPassRef,
    pass_evidence: ReviewPassEvidence,
    run: ReviewRunEvidence,
    external_object: ReviewKey,
}

impl ReviewExternalLinkAttachment {
    /// Constructs attachment evidence.
    pub const fn new(
        link: ReviewExternalLinkId,
        pass: ReviewPassRef,
        pass_evidence: ReviewPassEvidence,
        run: ReviewRunEvidence,
        external_object: ReviewKey,
    ) -> Self {
        Self {
            link,
            pass,
            pass_evidence,
            run,
            external_object,
        }
    }

    /// Returns the owning external-link reservation.
    pub const fn link(&self) -> ReviewExternalLinkId {
        self.link
    }

    /// Returns the producing pass.
    pub const fn pass(&self) -> ReviewPassRef {
        self.pass
    }

    /// Returns the canonical producing-pass evidence.
    pub const fn pass_evidence(&self) -> &ReviewPassEvidence {
        &self.pass_evidence
    }

    /// Returns the canonical producing-run evidence.
    pub const fn run_evidence(&self) -> ReviewRunEvidence {
        self.run
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkObservation {
    link: ReviewExternalLinkId,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassRef,
    pass_evidence: ReviewPassEvidence,
    run: ReviewRunEvidence,
    state: ReviewExternalObjectState,
}

impl ReviewExternalLinkObservation {
    /// Constructs one observation.
    pub const fn new(
        link: ReviewExternalLinkId,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassRef,
        pass_evidence: ReviewPassEvidence,
        run: ReviewRunEvidence,
        state: ReviewExternalObjectState,
    ) -> Self {
        Self {
            link,
            ordinal,
            pass,
            pass_evidence,
            run,
            state,
        }
    }

    /// Returns the observed external-link reservation.
    pub const fn link(&self) -> ReviewExternalLinkId {
        self.link
    }

    /// Returns the contiguous one-based ordinal.
    pub const fn ordinal(&self) -> ReviewEventOrdinal {
        self.ordinal
    }

    /// Returns the observing pass.
    pub const fn pass(&self) -> ReviewPassRef {
        self.pass
    }

    /// Returns the canonical observing-pass evidence.
    pub const fn pass_evidence(&self) -> &ReviewPassEvidence {
        &self.pass_evidence
    }

    /// Returns the canonical observing-run evidence.
    pub const fn run_evidence(&self) -> ReviewRunEvidence {
        self.run
    }

    /// Returns the reported state.
    pub const fn state(&self) -> ReviewExternalObjectState {
        self.state
    }
}

/// One durable result-bearing pass/run claim that appends no link child row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkClaim {
    pass: ReviewPassEvidence,
    run: ReviewRunEvidence,
}

impl ReviewExternalLinkClaim {
    /// Supplies the exact canonical pass and run evidence persisted for a
    /// no-change report or blocked publication.
    pub const fn new(pass: ReviewPassEvidence, run: ReviewRunEvidence) -> Self {
        Self { pass, run }
    }

    /// Returns the claiming pass.
    pub const fn pass(&self) -> ReviewPassRef {
        self.pass.reference()
    }

    /// Returns the canonical claiming-pass evidence.
    pub const fn pass_evidence(&self) -> &ReviewPassEvidence {
        &self.pass
    }

    /// Returns the canonical claiming-run evidence.
    pub const fn run_evidence(&self) -> ReviewRunEvidence {
        self.run
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
    claims: Vec<ReviewExternalLinkClaim>,
    pass_claims: BTreeMap<ReviewPassId, (ReviewPassEvidence, ReviewRunEvidence)>,
    run_claims: BTreeMap<ReviewRunId, (ReviewPassEvidence, ReviewRunEvidence)>,
}

impl ReviewExternalLink {
    /// Constructs the durable pre-effect reservation.
    pub fn try_reserve(
        id: ReviewExternalLinkId,
        association: ReviewExternalLinkAssociation,
        provider: ReviewKey,
        object_kind: ReviewExternalObjectKind,
        target: &ReviewTarget,
    ) -> Result<Self, ReviewExternalLinkTransitionFailure> {
        if association.target() != target.id() {
            return Err(ReviewExternalLinkTransitionFailure::ForeignAssociationTarget);
        }
        if &provider != target.provider() {
            return Err(ReviewExternalLinkTransitionFailure::ProviderMismatch);
        }
        Ok(Self {
            id,
            association,
            provider,
            object_kind,
            attachment: None,
            observations: Vec::new(),
            claims: Vec::new(),
            pass_claims: BTreeMap::new(),
            run_claims: BTreeMap::new(),
        })
    }

    /// Reconstitutes a complete link by validating attachment and observations.
    #[allow(clippy::too_many_arguments)]
    pub fn try_reconstitute(
        id: ReviewExternalLinkId,
        association: ReviewExternalLinkAssociation,
        provider: ReviewKey,
        object_kind: ReviewExternalObjectKind,
        attachment: Option<ReviewExternalLinkAttachment>,
        observations: Vec<ReviewExternalLinkObservation>,
        claims: Vec<ReviewExternalLinkClaim>,
        target: &ReviewTarget,
    ) -> Result<Self, ReviewExternalLinkTransitionFailure> {
        let mut link = Self::try_reserve(id, association, provider, object_kind, target)?;
        if let Some(attachment) = attachment {
            link = link.attach(attachment).map_err(|error| error.failure)?;
        }
        for observation in observations {
            link = link.observe(observation).map_err(|error| error.failure)?;
        }
        for claim in claims {
            link = link.replay_claim(claim)?;
        }
        Ok(link)
    }

    fn transition_error(
        &self,
        failure: ReviewExternalLinkTransitionFailure,
    ) -> ReviewExternalLinkTransitionError {
        ReviewExternalLinkTransitionError {
            current: Box::new(self.clone()),
            failure,
        }
    }

    fn claim_failure(
        &self,
        pass: &ReviewPassEvidence,
        run: ReviewRunEvidence,
    ) -> Option<ReviewExternalLinkTransitionFailure> {
        if self
            .pass_claims
            .get(&pass.reference().pass())
            .is_some_and(|(prior_pass, prior_run)| prior_pass != pass || *prior_run != run)
        {
            return Some(ReviewExternalLinkTransitionFailure::ConflictingPassEvidence);
        }
        self.run_claims
            .get(&run.reference().run())
            .is_some_and(|(prior_pass, prior_run)| prior_pass != pass || *prior_run != run)
            .then_some(ReviewExternalLinkTransitionFailure::ConflictingRunEvidence)
    }

    fn record_claim(&mut self, pass: &ReviewPassEvidence, run: ReviewRunEvidence) {
        let claim = (pass.clone(), run);
        self.pass_claims
            .entry(pass.reference().pass())
            .or_insert_with(|| claim.clone());
        self.run_claims
            .entry(run.reference().run())
            .or_insert(claim);
    }

    fn record_durable_claim(&mut self, claim: ReviewExternalLinkClaim) {
        self.record_claim(&claim.pass, claim.run);
        let pass = claim.pass().pass();
        match self
            .claims
            .binary_search_by_key(&pass, |existing| existing.pass().pass())
        {
            Ok(_) => {}
            Err(index) => self.claims.insert(index, claim),
        }
    }

    fn replay_claim(
        mut self,
        claim: ReviewExternalLinkClaim,
    ) -> Result<Self, ReviewExternalLinkTransitionFailure> {
        if claim.pass.reference().target() != self.association.target() {
            return Err(ReviewExternalLinkTransitionFailure::ForeignPass);
        }
        let publication_block = claim.pass.kind() == ReviewPassKind::Publish
            && matches!(claim.pass.state(), ReviewPassState::Blocked { .. });
        if !run_evidence_matches_pass(claim.run, &claim.pass) {
            return Err(if publication_block {
                ReviewExternalLinkTransitionFailure::IncompatiblePublicationBlockRunEvidence
            } else {
                ReviewExternalLinkTransitionFailure::IncompatibleObservationRunEvidence
            });
        }
        let compatible = match claim.pass.state() {
            ReviewPassState::Succeeded {
                result: Some(ReviewPassResult::ExternalLinkNoChange(result)),
                ..
            } => {
                claim.pass.kind() == ReviewPassKind::ImportExternalContext
                    && result.link() == self.id
                    && self.attachment.is_some()
                    && self.observations.iter().any(|observation| {
                        observation.ordinal == result.observed_through()
                            && observation.state == result.state()
                    })
            }
            ReviewPassState::Blocked {
                result: Some(_), ..
            } => publication_block_matches_link(&claim.pass, self.association, self.id),
            _ => false,
        };
        if !compatible {
            return Err(if publication_block {
                ReviewExternalLinkTransitionFailure::IncompatiblePublicationBlockPass
            } else {
                ReviewExternalLinkTransitionFailure::IncompatibleObservationPass
            });
        }
        if let Some(failure) = self.claim_failure(&claim.pass, claim.run) {
            return Err(failure);
        }
        self.record_durable_claim(claim);
        Ok(self)
    }

    /// Attaches the exact external identity after reservation.
    pub fn attach(
        mut self,
        attachment: ReviewExternalLinkAttachment,
    ) -> Result<Self, ReviewExternalLinkTransitionError> {
        if self.attachment.is_some() {
            return Err(self.transition_error(ReviewExternalLinkTransitionFailure::AlreadyAttached));
        }
        if attachment.link != self.id {
            return Err(
                self.transition_error(ReviewExternalLinkTransitionFailure::ForeignAttachmentLink)
            );
        }
        if attachment.pass != attachment.pass_evidence.reference() {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::AttachmentPassEvidenceMismatch,
            ));
        }
        if attachment.pass.target() != self.association.target() {
            return Err(self.transition_error(ReviewExternalLinkTransitionFailure::ForeignPass));
        }
        if !run_evidence_matches_pass(attachment.run, &attachment.pass_evidence) {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::IncompatibleAttachmentRunEvidence,
            ));
        }
        if let Some(failure) = self.claim_failure(&attachment.pass_evidence, attachment.run) {
            return Err(self.transition_error(failure));
        }
        let Some(result) = (match attachment.pass_evidence.state() {
            ReviewPassState::Succeeded {
                result: Some(ReviewPassResult::ExternalLinkAttachment(result)),
                ..
            } => Some(result),
            _ => None,
        }) else {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::IncompatibleAttachmentPass,
            ));
        };
        if !matches!(
            attachment.pass_evidence.kind(),
            ReviewPassKind::Publish | ReviewPassKind::ImportExternalContext
        ) || result.link() != self.id
            || result.external_object() != &attachment.external_object
            || result.finding_event().is_some_and(|event| {
                self.association != ReviewExternalLinkAssociation::Finding(event.finding())
                    || !matches!(
                        event.kind(),
                        ReviewFindingEventResultKind::Posted { link } if *link == self.id
                    )
                    || !matches!(
                        self.object_kind,
                        ReviewExternalObjectKind::Review
                            | ReviewExternalObjectKind::ReviewThread
                            | ReviewExternalObjectKind::ReviewComment
                            | ReviewExternalObjectKind::ChangeRequestComment
                    )
                    || !finding_event_result_matches_pass(
                        event.kind(),
                        attachment.pass_evidence.kind(),
                        ReviewPassTurnOutcome::Completed,
                    )
            })
        {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::IncompatibleAttachmentPass,
            ));
        }
        self.record_claim(&attachment.pass_evidence, attachment.run);
        self.attachment = Some(attachment);
        Ok(self)
    }

    /// Appends one same-target contiguous observation after attachment.
    pub fn observe(
        mut self,
        observation: ReviewExternalLinkObservation,
    ) -> Result<Self, ReviewExternalLinkTransitionError> {
        if self.attachment.is_none() {
            return Err(self.transition_error(ReviewExternalLinkTransitionFailure::NotAttached));
        }
        if observation.link != self.id {
            return Err(
                self.transition_error(ReviewExternalLinkTransitionFailure::ForeignObservationLink)
            );
        }
        if observation.pass != observation.pass_evidence.reference() {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::ObservationPassEvidenceMismatch,
            ));
        }
        if observation.pass.target() != self.association.target() {
            return Err(self.transition_error(ReviewExternalLinkTransitionFailure::ForeignPass));
        }
        if !run_evidence_matches_pass(observation.run, &observation.pass_evidence) {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::IncompatibleObservationRunEvidence,
            ));
        }
        if observation.pass_evidence.kind() != ReviewPassKind::ImportExternalContext
            || !matches!(
                observation.pass_evidence.state(),
                ReviewPassState::Succeeded {
                    result: Some(ReviewPassResult::ExternalLinkObservation(result)),
                    ..
                } if result.link() == self.id
                    && result.ordinal() == observation.ordinal
                    && result.state() == observation.state
            )
        {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::IncompatibleObservationPass,
            ));
        }
        if self
            .observations
            .last()
            .is_some_and(|previous| previous.state == observation.state)
        {
            return Err(
                self.transition_error(ReviewExternalLinkTransitionFailure::UnchangedObservation)
            );
        }
        let expected = match self.observations.last() {
            Some(previous) => previous.ordinal.checked_successor(),
            None => Some(ReviewEventOrdinal::one()),
        };
        if expected != Some(observation.ordinal) {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::NoncontiguousOrdinal { expected },
            ));
        }
        if let Some(failure) = self.claim_failure(&observation.pass_evidence, observation.run) {
            return Err(self.transition_error(failure));
        }
        self.record_claim(&observation.pass_evidence, observation.run);
        self.observations.push(observation);
        Ok(self)
    }

    /// Confirms that one import pass reported the latest durable state without
    /// appending another observation.
    pub fn confirm_unchanged(
        mut self,
        pass: ReviewPassEvidence,
        run: ReviewRunEvidence,
    ) -> Result<Self, ReviewExternalLinkTransitionError> {
        if self.attachment.is_none() {
            return Err(self.transition_error(ReviewExternalLinkTransitionFailure::NotAttached));
        }
        if pass.reference().target() != self.association.target() {
            return Err(self.transition_error(ReviewExternalLinkTransitionFailure::ForeignPass));
        }
        if !run_evidence_matches_pass(run, &pass) {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::IncompatibleObservationRunEvidence,
            ));
        }
        let Some(result) = (match pass.state() {
            ReviewPassState::Succeeded {
                result: Some(ReviewPassResult::ExternalLinkNoChange(result)),
                ..
            } => Some(result),
            _ => None,
        }) else {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::IncompatibleObservationPass,
            ));
        };
        if pass.kind() != ReviewPassKind::ImportExternalContext
            || result.link() != self.id
            || self.observations.last().is_none_or(|previous| {
                previous.ordinal != result.observed_through() || previous.state != result.state()
            })
        {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::IncompatibleObservationPass,
            ));
        }
        if let Some(failure) = self.claim_failure(&pass, run) {
            return Err(self.transition_error(failure));
        }
        self.record_durable_claim(ReviewExternalLinkClaim::new(pass, run));
        Ok(self)
    }

    /// Authenticates one blocked publication against this exact pending
    /// reservation.
    pub fn block_publication(
        mut self,
        pass: ReviewPassEvidence,
        run: ReviewRunEvidence,
    ) -> Result<Self, ReviewExternalLinkTransitionError> {
        if self.attachment.is_some() {
            return Err(self.transition_error(ReviewExternalLinkTransitionFailure::AlreadyAttached));
        }
        if pass.reference().target() != self.association.target() {
            return Err(self.transition_error(ReviewExternalLinkTransitionFailure::ForeignPass));
        }
        if !run_evidence_matches_pass(run, &pass) {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::IncompatiblePublicationBlockRunEvidence,
            ));
        }
        let compatible = publication_block_matches_link(&pass, self.association, self.id);
        if !compatible {
            return Err(self.transition_error(
                ReviewExternalLinkTransitionFailure::IncompatiblePublicationBlockPass,
            ));
        }
        if let Some(failure) = self.claim_failure(&pass, run) {
            return Err(self.transition_error(failure));
        }
        self.record_durable_claim(ReviewExternalLinkClaim::new(pass, run));
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

    /// Borrows canonical no-change and publication-block claims.
    pub fn claims(&self) -> &[ReviewExternalLinkClaim] {
        &self.claims
    }
}

fn publication_block_matches_link(
    pass: &ReviewPassEvidence,
    association: ReviewExternalLinkAssociation,
    link: ReviewExternalLinkId,
) -> bool {
    match pass.state() {
        ReviewPassState::Blocked {
            result: Some(ReviewPassResult::ExternalLinkPublicationBlocked(result)),
            ..
        } => pass.kind() == ReviewPassKind::Publish && result.link() == link,
        ReviewPassState::Blocked {
            result: Some(ReviewPassResult::FindingEvent(event)),
            ..
        } => {
            pass.kind() == ReviewPassKind::Publish
                && association == ReviewExternalLinkAssociation::Finding(event.finding())
                && matches!(
                    event.kind(),
                    ReviewFindingEventResultKind::BlockedWithReason {
                        link: Some(result_link),
                        ..
                    } if *result_link == link
                )
        }
        _ => false,
    }
}

/// Canonical claim that one attached provider object belongs to one target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewExternalObjectClaim {
    target: ReviewTargetId,
    provider: ReviewKey,
    repository: ReviewKey,
    subject: ReviewTargetSubject,
    object_kind: ReviewExternalObjectKind,
    external_object: ReviewKey,
}

impl ReviewExternalObjectClaim {
    /// Derives a claim from one canonical target and its attached link.
    pub fn try_new(
        link: &ReviewExternalLink,
        target: &ReviewTarget,
    ) -> Result<Self, ReviewExternalObjectClaimError> {
        if link.association.target() != target.id() {
            return Err(ReviewExternalObjectClaimError::ForeignTarget);
        }
        if &link.provider != target.provider() {
            return Err(ReviewExternalObjectClaimError::ProviderMismatch);
        }
        let Some(attachment) = link.attachment.as_ref() else {
            return Err(ReviewExternalObjectClaimError::NotAttached);
        };
        Ok(Self {
            target: target.id(),
            provider: target.provider().clone(),
            repository: target.repository().clone(),
            subject: target.subject(),
            object_kind: link.object_kind,
            external_object: attachment.external_object.clone(),
        })
    }

    /// Validates reuse of this canonical object by a refreshed target snapshot.
    pub fn validate_reassociation(
        &self,
        candidate: &Self,
    ) -> Result<(), ReviewExternalObjectClaimError> {
        if self.provider != candidate.provider
            || self.object_kind != candidate.object_kind
            || self.external_object != candidate.external_object
        {
            return Err(ReviewExternalObjectClaimError::DifferentObject);
        }
        if self.target == candidate.target {
            return Err(ReviewExternalObjectClaimError::SameTarget);
        }
        let same_change_request = matches!(
            (self.subject, candidate.subject),
            (
                ReviewTargetSubject::ChangeRequest(existing),
                ReviewTargetSubject::ChangeRequest(candidate)
            ) if existing == candidate
        );
        if self.repository != candidate.repository || !same_change_request {
            return Err(ReviewExternalObjectClaimError::UnrelatedTarget);
        }
        Ok(())
    }

    /// Returns the exact target snapshot.
    pub const fn target(&self) -> ReviewTargetId {
        self.target
    }

    /// Borrows the canonical provider-wide object key.
    pub const fn external_object(&self) -> &ReviewKey {
        &self.external_object
    }
}

/// Why an external-object target claim or reassociation is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewExternalObjectClaimError {
    /// The canonical target does not own the link association.
    ForeignTarget,
    /// The canonical target contradicts the reservation's frozen provider.
    ProviderMismatch,
    /// The link is still an unattached reservation.
    NotAttached,
    /// The compared claims name different canonical provider objects.
    DifferentObject,
    /// A second claim repeats the same exact target snapshot.
    SameTarget,
    /// The repeated object belongs to another commit or change request.
    UnrelatedTarget,
}

/// Rejected external-link transition retaining the unchanged aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkTransitionError {
    current: Box<ReviewExternalLink>,
    failure: ReviewExternalLinkTransitionFailure,
}

impl ReviewExternalLinkTransitionError {
    /// Borrows the unchanged link rejected by the transition.
    pub const fn current(&self) -> &ReviewExternalLink {
        &self.current
    }

    /// Returns why the transition was rejected.
    pub const fn failure(&self) -> ReviewExternalLinkTransitionFailure {
        self.failure
    }

    /// Returns the unchanged link and transition failure.
    pub fn into_parts(self) -> (ReviewExternalLink, ReviewExternalLinkTransitionFailure) {
        (*self.current, self.failure)
    }
}

/// Why external-link construction, transition, or reconstitution failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewExternalLinkTransitionFailure {
    /// The canonical target does not own the reservation association.
    ForeignAssociationTarget,
    /// The reservation provider differs from its canonical target provider.
    ProviderMismatch,
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
    /// The stored attachment pass identity contradicts its canonical evidence.
    AttachmentPassEvidenceMismatch,
    /// Attachment pass evidence contradicts its independently loaded run.
    IncompatibleAttachmentRunEvidence,
    /// Observation evidence did not canonically succeed in an import pass.
    IncompatibleObservationPass,
    /// The stored observation pass identity contradicts its canonical evidence.
    ObservationPassEvidenceMismatch,
    /// Observation pass evidence contradicts its independently loaded run.
    IncompatibleObservationRunEvidence,
    /// Publication-block evidence did not name this pending reservation.
    IncompatiblePublicationBlockPass,
    /// Publication-block pass evidence contradicts its independently loaded run.
    IncompatiblePublicationBlockRunEvidence,
    /// The reported state equals the latest durable observation.
    UnchangedObservation,
    /// One reused pass identity carries contradictory canonical evidence.
    ConflictingPassEvidence,
    /// One reused run identity carries another pass, workflow, or policy.
    ConflictingRunEvidence,
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
mod test_support;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use explicit fixture expectations"
)]
mod tests;
