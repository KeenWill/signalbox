//! Review finding for `docs/spec/review-workflows.md`.

use crate::ReviewExternalLinkId;
use crate::ReviewPassId;
use crate::ReviewRunId;
use crate::ReviewTargetId;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroU32;

use super::{
    ReviewConfidence, ReviewEventOrdinal, ReviewExternalLink, ReviewExternalLinkAssociation,
    ReviewExternalObjectKind, ReviewFindingConfidenceAxes, ReviewFindingEventResult,
    ReviewFindingEventResultKind, ReviewFindingEventType, ReviewFindingRef, ReviewFindingStatus,
    ReviewKey, ReviewPassEvidence, ReviewPassKind, ReviewPassRef, ReviewPassResult,
    ReviewPassState, ReviewReferencedFindingEvidence, ReviewRunEvidence, ReviewRunState,
    ReviewTarget, ReviewText, ReviewWorkflowKind, referenced_finding_status_is_eligible,
    run_evidence_matches_pass,
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
    pub(super) reference: ReviewFindingRef,
    pub(super) producing_pass: ReviewPassEvidence,
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

    pub(super) fn result_kind(&self) -> ReviewFindingEventResultKind {
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
    pub(super) proposal: ReviewFindingProposal,
    events: Vec<ReviewFindingEvent>,
    pub(super) status: ReviewFindingStatus,
    pub(super) pass_claims: BTreeMap<ReviewPassId, (ReviewPassEvidence, ReviewRunEvidence)>,
    pub(super) run_claims: BTreeMap<ReviewRunId, (ReviewPassEvidence, ReviewRunEvidence)>,
    pub(super) publication_links: BTreeSet<ReviewExternalLinkId>,
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

pub(super) fn finding_transition(
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
