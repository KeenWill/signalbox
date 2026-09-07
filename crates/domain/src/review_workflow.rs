//! Review-workflow aggregates above session execution.
//!
//! The normative specification is `docs/spec/review-workflows.md`.

mod external_link;
pub use external_link::{
    ReviewExternalLink, ReviewExternalLinkAssociation, ReviewExternalLinkAttachment,
    ReviewExternalLinkClaim, ReviewExternalLinkObservation, ReviewExternalLinkTransitionError,
    ReviewExternalLinkTransitionFailure, ReviewExternalObjectClaim, ReviewExternalObjectClaimError,
    ReviewExternalObjectKind, ReviewExternalObjectState,
};

mod finding;
pub use finding::{
    ReviewFinding, ReviewFindingContent, ReviewFindingDiffSide, ReviewFindingEvent,
    ReviewFindingEventKind, ReviewFindingExternalLinkError, ReviewFindingExternalLinkFailure,
    ReviewFindingExternalLinkRef, ReviewFindingLocation, ReviewFindingPendingExternalLinkRef,
    ReviewFindingProposal, ReviewFindingReferenceGraphError, ReviewFindingSeverity,
    ReviewFindingTransitionError, ReviewFindingTransitionFailure, ReviewLineRange,
    ReviewLineRangeError, validate_complete_review_finding_reference_graph,
};

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

const REVIEW_PRODUCED_FINDINGS_MAXIMUM: usize = 32;

mod policy;
mod value;

pub use policy::{ReviewPolicy, ReviewPolicyError, ReviewPolicyVersion};
pub use value::{
    ReviewChangeRequestNumber, ReviewConfidence, ReviewConfidenceError, ReviewEventOrdinal,
    ReviewFindingConfidenceAxes, ReviewKey, ReviewPositiveNumberError, ReviewText,
    ReviewValueError, ReviewValueFailure,
};

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
