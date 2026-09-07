//! Review-workflow aggregates above session execution.
//!
//! The normative specification is `docs/spec/review-workflows.md`.

mod pass;
pub use pass::{
    ReviewPass, ReviewPassAcceptedInputEvidence, ReviewPassConstructionError,
    ReviewPassConstructionFailure, ReviewPassKind, ReviewPassReconstitutionError,
    ReviewPassReconstitutionFailure, ReviewPassReconstitutionInput, ReviewPassState,
    ReviewPassTransitionError, ReviewPassTransitionFailure, ReviewPassTurnEvidence,
    ReviewPassTurnOutcome,
};
use pass::{finding_event_result_matches_pass, referenced_finding_result};

mod run;
pub use run::{
    ReviewPassEvidence, ReviewRun, ReviewRunEvidence, ReviewRunEvidenceFailure,
    ReviewRunReconstitutionError, ReviewRunReconstitutionInput, ReviewRunState,
    ReviewRunTransitionError, ReviewRunTransitionFailure, ReviewWorkflowKind,
};
use run::{run_evidence_matches_pass, workflow_matches_pass_kind};

mod pass_result;
use pass_result::referenced_finding_status_is_eligible;
pub use pass_result::{
    ReviewExternalLinkAttachmentResult, ReviewExternalLinkNoChangeResult,
    ReviewExternalLinkObservationResult, ReviewExternalLinkPublicationBlockedResult,
    ReviewFindingEventResult, ReviewFindingEventResultKind, ReviewFindingEventType,
    ReviewFindingStatus, ReviewPassResult, ReviewProducedFindings, ReviewProducedFindingsError,
    ReviewReferencedFindingEvidence,
};

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
