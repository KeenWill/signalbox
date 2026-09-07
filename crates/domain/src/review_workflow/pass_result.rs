//! Review pass result for `docs/spec/review-workflows.md`.

use crate::ReviewExternalLinkId;

use super::{
    REVIEW_PRODUCED_FINDINGS_MAXIMUM, ReviewEventOrdinal, ReviewExternalObjectState, ReviewFinding,
    ReviewFindingRef, ReviewKey, ReviewPassEvidence, ReviewPassKind, ReviewPassRef,
    ReviewPassState, ReviewPolicy, ReviewRunEvidence, ReviewText, ReviewWorkflowKind,
    run_evidence_matches_pass,
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
    pub(super) reference: ReviewFindingRef,
    pub(super) status: ReviewFindingStatus,
    pub(super) producer_policy: ReviewPolicy,
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

pub(super) const fn referenced_finding_status_is_eligible(status: ReviewFindingStatus) -> bool {
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
