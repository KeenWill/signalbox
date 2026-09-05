//! Review-workflow aggregates above session execution.
//!
//! The normative specification is `docs/spec/review-workflows.md`.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU32, NonZeroU64},
};

use crate::{
    AcceptedInputId, ContextFrontierId, ReviewExternalLinkId, ReviewFindingId, ReviewPassId,
    ReviewRunId, ReviewTargetId, SessionId, TurnId,
};

const REVIEW_KEY_MAXIMUM_BYTES: usize = 1_024;
const REVIEW_TEXT_MAXIMUM_BYTES: usize = 65_536;
const REVIEW_CONFIDENCE_MAXIMUM_BASIS_POINTS: u16 = 10_000;
const REVIEW_POLICY_VERSION_ONE_MINIMUM_JUDGE_BASIS_POINTS: u16 = 7_000;
const REVIEW_POLICY_VERSION_ONE_MINIMUM_PUBLICATION_BASIS_POINTS: u16 = 8_000;
const REVIEW_PRODUCED_FINDINGS_MAXIMUM: usize = 32;

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

/// The independent confidence axes carried by one review finding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReviewFindingConfidenceAxes {
    is_real_confidence: ReviewConfidence,
    severity_label_confidence: ReviewConfidence,
}

impl ReviewFindingConfidenceAxes {
    /// Creates explicitly labeled is-real and severity-label confidence axes.
    pub const fn new(
        is_real_confidence: ReviewConfidence,
        severity_label_confidence: ReviewConfidence,
    ) -> Self {
        Self {
            is_real_confidence,
            severity_label_confidence,
        }
    }

    /// Returns confidence that the issue is real.
    pub const fn is_real_confidence(self) -> ReviewConfidence {
        self.is_real_confidence
    }

    /// Returns confidence that the severity label is correct.
    pub const fn severity_label_confidence(self) -> ReviewConfidence {
        self.severity_label_confidence
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
        let is_unsupported_version = version.get() != ReviewPolicyVersion::one().get();
        let is_noncanonical_version_one = minimum_judge_confidence.basis_points()
            != REVIEW_POLICY_VERSION_ONE_MINIMUM_JUDGE_BASIS_POINTS
            || minimum_publication_confidence.basis_points()
                != REVIEW_POLICY_VERSION_ONE_MINIMUM_PUBLICATION_BASIS_POINTS;
        if is_unsupported_version
            || is_noncanonical_version_one
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewChangeRequestIdentity {
    provider: ReviewKey,
    repository: ReviewKey,
    number: ReviewChangeRequestNumber,
}

/// Canonical topology facts for one review target's immediate stack parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewTargetParentRef {
    target: ReviewTargetId,
    provider: ReviewKey,
    repository: ReviewKey,
    head_revision: ReviewKey,
}

impl ReviewTargetParentRef {
    fn from_target(target: &ReviewTarget) -> Self {
        Self {
            target: target.id,
            provider: target.provider.clone(),
            repository: target.repository.clone(),
            head_revision: target.head_revision.clone(),
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

    /// Borrows the parent's frozen head revision.
    pub const fn head_revision(&self) -> &ReviewKey {
        &self.head_revision
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
    ancestry: Vec<ReviewTargetId>,
    change_request_ancestry: Vec<ReviewChangeRequestIdentity>,
}

impl ReviewTarget {
    /// Constructs a target snapshot, rejecting incomplete comparison evidence
    /// or an unauthenticated parent edge.
    pub fn try_new(
        id: ReviewTargetId,
        provider: ReviewKey,
        repository: ReviewKey,
        subject: ReviewTargetSubject,
        head_revision: ReviewKey,
        base_revision: Option<ReviewKey>,
        stack_parent: Option<&ReviewTarget>,
    ) -> Result<Self, ReviewTargetError> {
        if matches!(subject, ReviewTargetSubject::ChangeRequest(_)) && base_revision.is_none() {
            return Err(ReviewTargetError::MissingChangeRequestBase { target: id });
        }
        if let Some(parent) = stack_parent {
            if parent.id == id {
                return Err(ReviewTargetError::SelfParent { target: id });
            }
            if parent.ancestry.contains(&id) {
                return Err(ReviewTargetError::CyclicParent { target: id });
            }
            if parent.provider != provider || parent.repository != repository {
                return Err(ReviewTargetError::ForeignParent { target: id });
            }
            let Some(base_revision) = &base_revision else {
                return Err(ReviewTargetError::MissingParentBase { target: id });
            };
            if parent.head_revision != *base_revision {
                return Err(ReviewTargetError::DisconnectedParent { target: id });
            }
            if let ReviewTargetSubject::ChangeRequest(number) = subject {
                let identity = ReviewChangeRequestIdentity {
                    provider: provider.clone(),
                    repository: repository.clone(),
                    number,
                };
                if parent.change_request_ancestry.contains(&identity) {
                    return Err(ReviewTargetError::RepeatedChangeRequest { target: id });
                }
            }
        }
        let ancestry = stack_parent.map_or_else(Vec::new, |parent| {
            let mut ancestry = Vec::with_capacity(parent.ancestry.len() + 1);
            ancestry.push(parent.id);
            ancestry.extend_from_slice(&parent.ancestry);
            ancestry
        });
        let mut change_request_ancestry = Vec::new();
        if let ReviewTargetSubject::ChangeRequest(number) = subject {
            change_request_ancestry.push(ReviewChangeRequestIdentity {
                provider: provider.clone(),
                repository: repository.clone(),
                number,
            });
        }
        if let Some(parent) = stack_parent {
            change_request_ancestry.extend_from_slice(&parent.change_request_ancestry);
        }
        Ok(Self {
            id,
            provider,
            repository,
            subject,
            head_revision,
            base_revision,
            stack_parent: stack_parent.map(ReviewTargetParentRef::from_target),
            ancestry,
            change_request_ancestry,
        })
    }

    /// Reconstitutes a target snapshot after authenticating the parent
    /// identity stored on the child row against the supplied canonical parent.
    #[allow(clippy::too_many_arguments)]
    pub fn try_reconstitute(
        id: ReviewTargetId,
        provider: ReviewKey,
        repository: ReviewKey,
        subject: ReviewTargetSubject,
        head_revision: ReviewKey,
        base_revision: Option<ReviewKey>,
        stack_parent: Option<ReviewTargetId>,
        stack_parent_evidence: Option<&ReviewTarget>,
    ) -> Result<Self, ReviewTargetError> {
        if stack_parent != stack_parent_evidence.map(Self::id) {
            return Err(ReviewTargetError::ParentIdentityMismatch { target: id });
        }
        Self::try_new(
            id,
            provider,
            repository,
            subject,
            head_revision,
            base_revision,
            stack_parent_evidence,
        )
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

    /// Borrows the complete nearest-first canonical parent chain.
    pub fn ancestry(&self) -> &[ReviewTargetId] {
        &self.ancestry
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
    /// The complete parent chain repeats the target.
    CyclicParent {
        /// The target whose proposed chain contains itself.
        target: ReviewTargetId,
    },
    /// The parent belongs to another provider or repository.
    ForeignParent {
        /// The child target whose topology was rejected.
        target: ReviewTargetId,
    },
    /// A parented target lacks the exact comparison revision.
    MissingParentBase {
        /// The child target whose topology was rejected.
        target: ReviewTargetId,
    },
    /// The child's comparison revision is not its canonical parent's head.
    DisconnectedParent {
        /// The child target whose topology was rejected.
        target: ReviewTargetId,
    },
    /// One stack chain contains two snapshots of the same change request.
    RepeatedChangeRequest {
        /// The child snapshot whose proposed chain repeats its logical subject.
        target: ReviewTargetId,
    },
    /// The parent identity stored on the child row differs from the supplied
    /// canonical parent evidence.
    ParentIdentityMismatch {
        /// The child snapshot whose stored parent edge was rejected.
        target: ReviewTargetId,
    },
}

/// A target-bound review-run reference.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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
mod tests {
    use expect_test::expect;
    use signalbox_expect_table::table;
    use uuid::Uuid;

    use super::*;

    /// Target identity repeated to close the ancestry cycle under test.
    const CYCLIC_ROOT_TARGET: u128 = 90;
    /// Distinct intermediate target in the ancestry-cycle fixture.
    const CYCLIC_PARENT_TARGET: u128 = 91;
    /// Arbitrary root revision; only exact continuity with the parent matters.
    const CYCLIC_ROOT_HEAD: &str = "cyclic-root-head";
    /// Arbitrary parent revision; only exact continuity with the child matters.
    const CYCLIC_PARENT_HEAD: &str = "cyclic-parent-head";
    /// Arbitrary proposed child revision in the rejected cycle.
    const CYCLIC_CHILD_HEAD: &str = "cyclic-child-head";
    /// Canonical target shared by the ordinary review-workflow fixtures.
    const CANONICAL_TARGET_SEED: u128 = 1;
    /// Producing pass used by the canonical finding fixture.
    const PRODUCING_PASS_SEED: u128 = 3;
    /// Namespace separating pass-derived run identities from pass identities.
    const PASS_RUN_NAMESPACE: u128 = 10_000;
    /// Canonical finding used by ordinary finding-history fixtures.
    const CANONICAL_FINDING_SEED: u128 = 10;
    /// Distinct finding used to exercise foreign-owner rejection.
    const FOREIGN_FINDING_SEED: u128 = 11;
    /// Arbitrary judgment pass used by unchanged-aggregate fixtures.
    const ARBITRARY_JUDGMENT_PASS_SEED: u128 = 20;
    /// Distinct pass used to exercise changed user-global pass claims.
    const REASSIGNED_PASS_SEED: u128 = 21;
    /// Distinct event pass used by referenced-finding fixtures.
    const ARBITRARY_DEDUPE_PASS_SEED: u128 = 22;
    /// Arbitrary turn retained by deliberately cross-wired pass evidence.
    const CROSS_WIRED_TURN_SEED: u128 = 121;
    /// Arbitrary external-link reservation used by transition fixtures.
    const ARBITRARY_LINK_SEED: u128 = 30;

    /// Maps an arbitrary seed to a deterministic target identity; equality and
    /// difference between seeds are the only default semantics.
    fn target_id(value: u128) -> ReviewTargetId {
        ReviewTargetId::from_uuid(Uuid::from_u128(value))
    }

    /// Maps an arbitrary seed to a deterministic run identity; equality and
    /// difference between seeds are the only default semantics.
    fn run_id(value: u128) -> ReviewRunId {
        ReviewRunId::from_uuid(Uuid::from_u128(value))
    }

    /// Maps an arbitrary seed to a deterministic pass identity; equality and
    /// difference between seeds are the only default semantics.
    fn pass_id(value: u128) -> ReviewPassId {
        ReviewPassId::from_uuid(Uuid::from_u128(value))
    }

    /// Maps an arbitrary seed to a deterministic finding identity; equality
    /// and difference between seeds are the only default semantics.
    fn finding_id(value: u128) -> ReviewFindingId {
        ReviewFindingId::from_uuid(Uuid::from_u128(value))
    }

    /// Maps an arbitrary seed to a deterministic external-link identity;
    /// equality and difference between seeds are the only default semantics.
    fn link_id(value: u128) -> ReviewExternalLinkId {
        ReviewExternalLinkId::from_uuid(Uuid::from_u128(value))
    }

    /// Maps an arbitrary seed to a deterministic session identity; equality
    /// and difference between seeds are the only default semantics.
    fn session_id(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    /// Maps an arbitrary seed to a deterministic accepted-input identity;
    /// equality and difference between seeds are the only default semantics.
    fn accepted_input_id(value: u128) -> AcceptedInputId {
        AcceptedInputId::from_uuid(Uuid::from_u128(value))
    }

    /// Maps an arbitrary seed to a deterministic turn identity; equality and
    /// difference between seeds are the only default semantics.
    fn turn_id(value: u128) -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(value))
    }

    /// Maps an arbitrary seed to a deterministic frontier identity; equality
    /// and difference between seeds are the only default semantics.
    fn frontier_id(value: u128) -> ContextFrontierId {
        ContextFrontierId::from_uuid(Uuid::from_u128(value))
    }

    /// Admits exact fixture key content; repeated and distinct strings model
    /// key equality and difference unless a test asserts the content.
    fn key(value: &str) -> ReviewKey {
        ReviewKey::try_new(String::from(value)).expect("fixture key is valid")
    }

    /// Admits exact fixture narrative content; repeated and distinct strings
    /// model text equality and difference unless a test asserts the content.
    fn text(value: &str) -> ReviewText {
        ReviewText::try_new(String::from(value)).expect("fixture text is valid")
    }

    fn run_ref() -> ReviewRunRef {
        pass_ref(PRODUCING_PASS_SEED).run()
    }

    fn run_evidence() -> ReviewRunEvidence {
        ReviewRunEvidence::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            ReviewRunState::Succeeded {
                concluding_pass: pass_ref(PRODUCING_PASS_SEED),
            },
        )
    }

    fn unsupported_policy() -> ReviewPolicy {
        ReviewPolicy {
            version: ReviewPolicyVersion::try_new(2).expect("positive version"),
            minimum_judge_confidence: ReviewConfidence::try_from_basis_points(7_000)
                .expect("bounded confidence"),
            minimum_publication_confidence: ReviewConfidence::try_from_basis_points(8_001)
                .expect("bounded confidence"),
        }
    }

    /// Builds one user-global pass from an arbitrary pass seed, deriving a
    /// distinct owning run in the test namespace.
    fn pass_ref(value: u128) -> ReviewPassRef {
        ReviewPassRef::new(
            ReviewRunRef::new(
                target_id(CANONICAL_TARGET_SEED),
                run_id(PASS_RUN_NAMESPACE + value),
            ),
            pass_id(value),
        )
    }

    /// Builds arbitrary terminal success evidence for a seeded pass identity;
    /// the pass kind is the behavioral knob under test.
    fn succeeded_pass(value: u128, kind: ReviewPassKind) -> ReviewPassEvidence {
        ReviewPassEvidence::new(
            pass_ref(value),
            kind,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(value + 100),
                output_frontier: frontier_id(value + 200),
                result: None,
            },
        )
    }

    /// Builds arbitrary blocked evidence for a seeded pass identity; the pass
    /// kind is the behavioral knob under test.
    fn blocked_pass(value: u128, kind: ReviewPassKind) -> ReviewPassEvidence {
        ReviewPassEvidence::new(
            pass_ref(value),
            kind,
            ReviewPolicy::version_one(),
            ReviewPassState::Blocked {
                turn: turn_id(value + 100),
                result: None,
            },
        )
    }

    fn produced_findings_pass(
        finding: ReviewFindingRef,
        pass: ReviewPassEvidence,
    ) -> ReviewPassEvidence {
        let state = match pass.state() {
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                ..
            } => ReviewPassState::Succeeded {
                turn: *turn,
                output_frontier: *output_frontier,
                result: Some(ReviewPassResult::ProducedFindings(
                    ReviewProducedFindings::try_new(vec![finding])
                        .expect("fixture finding inventory is canonical"),
                )),
            },
            other => other.clone(),
        };
        ReviewPassEvidence::new(pass.reference(), pass.kind(), pass.policy(), state)
    }

    fn workflow_for_pass(kind: ReviewPassKind) -> ReviewWorkflowKind {
        match kind {
            ReviewPassKind::ImportExternalContext => ReviewWorkflowKind::ImportExternalContext,
            ReviewPassKind::ReadOnlyReview => ReviewWorkflowKind::ReadOnlyReview,
            ReviewPassKind::Judge => ReviewWorkflowKind::JudgeFindings,
            ReviewPassKind::Dedupe => ReviewWorkflowKind::DedupeFindings,
            ReviewPassKind::Publish => ReviewWorkflowKind::PublishReview,
            ReviewPassKind::Fix => ReviewWorkflowKind::FixFindings,
            ReviewPassKind::PropagateStack => ReviewWorkflowKind::PropagateStack,
        }
    }

    fn pass_run_evidence(pass: &ReviewPassEvidence) -> ReviewRunEvidence {
        let reference = pass.reference();
        let state = match pass.state() {
            ReviewPassState::Queued => ReviewRunState::Queued,
            ReviewPassState::Running { .. } => ReviewRunState::Running {
                active_pass: reference,
            },
            ReviewPassState::Succeeded { .. } => ReviewRunState::Succeeded {
                concluding_pass: reference,
            },
            ReviewPassState::Failed { .. } => ReviewRunState::Failed {
                failed_pass: reference,
            },
            ReviewPassState::Blocked { .. } => ReviewRunState::Blocked {
                blocking_pass: reference,
            },
            ReviewPassState::Cancelled { .. } => ReviewRunState::Cancelled {
                last_pass: Some(reference),
            },
        };
        ReviewRunEvidence::new(
            reference.run(),
            workflow_for_pass(pass.kind()),
            pass.policy(),
            state,
        )
    }

    fn pass_with_finding_event(
        finding: ReviewFindingRef,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassEvidence,
        event_kind: &ReviewFindingEventKind,
    ) -> ReviewPassEvidence {
        let result = ReviewFindingEventResult::new(finding, ordinal, event_kind.result_kind());
        let state = match pass.state() {
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                result: current,
                ..
            } => {
                let result = match (event_kind, current) {
                    (
                        ReviewFindingEventKind::Posted { .. },
                        Some(ReviewPassResult::ExternalLinkAttachment(attachment)),
                    ) => ReviewPassResult::ExternalLinkAttachment(
                        ReviewExternalLinkAttachmentResult::new(
                            attachment.link(),
                            attachment.external_object().clone(),
                            Some(result),
                        ),
                    ),
                    _ => ReviewPassResult::FindingEvent(result),
                };
                ReviewPassState::Succeeded {
                    turn: *turn,
                    output_frontier: *output_frontier,
                    result: Some(result),
                }
            }
            ReviewPassState::Blocked { turn, .. } => ReviewPassState::Blocked {
                turn: *turn,
                result: Some(ReviewPassResult::FindingEvent(result)),
            },
            other => other.clone(),
        };
        ReviewPassEvidence::new(pass.reference(), pass.kind(), pass.policy(), state)
    }

    fn pass_with_attachment_result(
        pass: ReviewPassEvidence,
        link: ReviewExternalLinkId,
        external_object: &ReviewKey,
    ) -> ReviewPassEvidence {
        let state = match pass.state() {
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                ..
            } => ReviewPassState::Succeeded {
                turn: *turn,
                output_frontier: *output_frontier,
                result: Some(ReviewPassResult::ExternalLinkAttachment(
                    ReviewExternalLinkAttachmentResult::new(link, external_object.clone(), None),
                )),
            },
            other => other.clone(),
        };
        ReviewPassEvidence::new(pass.reference(), pass.kind(), pass.policy(), state)
    }

    fn pass_with_posted_attachment_result(
        finding: ReviewFindingRef,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassEvidence,
        link: ReviewExternalLinkId,
        external_object: &ReviewKey,
    ) -> ReviewPassEvidence {
        let event = ReviewFindingEventResult::new(
            finding,
            ordinal,
            ReviewFindingEventResultKind::Posted { link },
        );
        let state = match pass.state() {
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                ..
            } => ReviewPassState::Succeeded {
                turn: *turn,
                output_frontier: *output_frontier,
                result: Some(ReviewPassResult::ExternalLinkAttachment(
                    ReviewExternalLinkAttachmentResult::new(
                        link,
                        external_object.clone(),
                        Some(event),
                    ),
                )),
            },
            other => other.clone(),
        };
        ReviewPassEvidence::new(pass.reference(), pass.kind(), pass.policy(), state)
    }

    fn finding_event(
        finding: ReviewFindingRef,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassEvidence,
        kind: ReviewFindingEventKind,
    ) -> ReviewFindingEvent {
        let pass = pass_with_finding_event(finding, ordinal, pass, &kind);
        ReviewFindingEvent::new(
            finding,
            ordinal,
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            kind,
        )
    }

    fn posted_finding_event(
        finding: ReviewFindingRef,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassEvidence,
        link: ReviewExternalLinkId,
    ) -> ReviewFindingEvent {
        let external_object = key("external-comment-42");
        let pass =
            pass_with_posted_attachment_result(finding, ordinal, pass, link, &external_object);
        let aggregate = pending_link(
            link,
            ReviewExternalLinkAssociation::Finding(finding),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(ReviewExternalLinkAttachment::new(
            link,
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            external_object,
        ))
        .expect("posted result and attachment are admitted atomically");
        let link = ReviewFindingExternalLinkRef::try_new(finding, &aggregate)
            .expect("attached result supports the exact finding");
        ReviewFindingEvent::new(
            finding,
            ordinal,
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            ReviewFindingEventKind::Posted {
                link: Box::new(link),
            },
        )
    }

    /// Builds referenced-finding evidence from an arbitrary identity seed and
    /// the exact status relevant to the test.
    fn referenced_finding(
        value: u128,
        status: ReviewFindingStatus,
    ) -> ReviewReferencedFindingEvidence {
        ReviewReferencedFindingEvidence {
            reference: finding_ref(value),
            status,
            producer_policy: ReviewPolicy::version_one(),
        }
    }

    /// Builds a finding in the canonical producing pass from an arbitrary
    /// finding seed.
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

    struct FindingConfidenceAxes {
        is_real: u16,
        severity_label: u16,
    }

    fn finding_content_with_confidence_axes(
        diff_side: Option<ReviewFindingDiffSide>,
        confidence: FindingConfidenceAxes,
    ) -> ReviewFindingContent {
        ReviewFindingContent::new(
            ReviewFindingLocation::new(
                key("src/lib.rs"),
                Some(ReviewLineRange::try_new(4, 7).expect("ordered line range")),
                diff_side,
            ),
            text("Finding title"),
            text("Finding body"),
            ReviewFindingSeverity::High,
            ReviewFindingConfidenceAxes::new(
                ReviewConfidence::try_from_basis_points(confidence.is_real)
                    .expect("bounded is-real confidence"),
                ReviewConfidence::try_from_basis_points(confidence.severity_label)
                    .expect("bounded severity-label confidence"),
            ),
            key("correctness"),
            Some(text("Apply the exact fix")),
        )
    }

    fn finding_content(diff_side: Option<ReviewFindingDiffSide>) -> ReviewFindingContent {
        finding_content_with_confidence_axes(
            diff_side,
            FindingConfidenceAxes {
                is_real: 8_500,
                severity_label: 8_500,
            },
        )
    }

    fn proposal_with_confidence_axes(confidence: FindingConfidenceAxes) -> ReviewFindingProposal {
        let target = target_with_base();
        ReviewFindingProposal::try_new(
            finding_ref(10),
            produced_findings_pass(
                finding_ref(10),
                succeeded_pass(3, ReviewPassKind::ReadOnlyReview),
            ),
            run_evidence(),
            &target,
            finding_content_with_confidence_axes(Some(ReviewFindingDiffSide::Right), confidence),
        )
        .expect("producing pass belongs to the finding run")
    }

    fn proposal() -> ReviewFindingProposal {
        proposal_with_confidence_axes(FindingConfidenceAxes {
            is_real: 8_500,
            severity_label: 8_500,
        })
    }

    fn finding_ref_for_producer(
        target: ReviewTargetId,
        producing_pass_seed: u128,
    ) -> ReviewFindingRef {
        ReviewFindingRef::new(
            ReviewPassRef::new(
                ReviewRunRef::new(target, run_id(PASS_RUN_NAMESPACE + producing_pass_seed)),
                pass_id(producing_pass_seed),
            ),
            finding_id(20_000 + producing_pass_seed),
        )
    }

    fn open_finding_with_producer(
        reference: ReviewFindingRef,
        policy: ReviewPolicy,
    ) -> ReviewFinding {
        let target = ReviewTarget::try_new(
            reference.target(),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("head"),
            Some(key("base")),
            None,
        )
        .expect("fixture target has complete comparison evidence");
        let producing_pass = ReviewPassEvidence::new(
            reference.pass(),
            ReviewPassKind::ReadOnlyReview,
            policy,
            ReviewPassState::Succeeded {
                turn: turn_id(6),
                output_frontier: frontier_id(8),
                result: Some(ReviewPassResult::ProducedFindings(
                    ReviewProducedFindings::try_new(vec![reference])
                        .expect("fixture inventory is sealed and canonical"),
                )),
            },
        );
        let producing_run = pass_run_evidence(&producing_pass);
        let proposal = ReviewFindingProposal::try_new(
            reference,
            producing_pass,
            producing_run,
            &target,
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect("fixture producer authenticates the exact finding");
        ReviewFinding::new(proposal)
    }
    fn succeeded_review_pass() -> ReviewPass {
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
        )
        .expect("canonical input admits the pass")
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
        .expect("active canonical turn starts the pass")
        .transition(
            ReviewPassState::Succeeded {
                turn: turn_id(6),
                output_frontier: frontier_id(8),
                result: Some(ReviewPassResult::ProducedFindings(
                    ReviewProducedFindings::try_new(Vec::new())
                        .expect("empty fixture inventory is canonical"),
                )),
            },
            Some(ReviewPassTurnEvidence::new(
                turn_id(6),
                session_id(4),
                accepted_input_id(5),
                ReviewPassTurnOutcome::Completed,
                Some(frontier_id(8)),
            )),
        )
        .expect("completed canonical turn supports success")
    }

    fn succeeded_judgment_pass() -> ReviewPass {
        ReviewPass {
            reference: pass_ref(3),
            kind: ReviewPassKind::Judge,
            session: session_id(4),
            accepted_input: accepted_input_id(5),
            origin_turn: turn_id(6),
            state: ReviewPassState::Succeeded {
                turn: turn_id(6),
                output_frontier: frontier_id(8),
                result: None,
            },
        }
    }

    fn accepted_finding_result() -> ReviewPassResult {
        ReviewPassResult::FindingEvent(ReviewFindingEventResult::new(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            ReviewFindingEventResultKind::Accepted,
        ))
    }

    /// Labeled target fixture whose values are arbitrary except where repeated
    /// or distinguished to exercise logical-target continuity.
    struct ChangeRequestTargetFixture<'a> {
        target_seed: u128,
        provider: &'a str,
        repository: &'a str,
        number: u64,
        head: &'a str,
    }

    fn change_request_target(fixture: ChangeRequestTargetFixture<'_>) -> ReviewTarget {
        ReviewTarget::try_new(
            target_id(fixture.target_seed),
            key(fixture.provider),
            key(fixture.repository),
            ReviewTargetSubject::ChangeRequest(
                ReviewChangeRequestNumber::try_new(fixture.number)
                    .expect("positive fixture change-request number"),
            ),
            key(fixture.head),
            Some(key("base")),
            None,
        )
        .expect("fixture change-request target freezes its base")
    }

    /// Labeled attached-link fixture whose seeds are arbitrary except where
    /// repeated or distinguished to exercise canonical-object reassociation.
    struct AttachedTargetLinkFixture<'a> {
        link_seed: u128,
        run_seed: u128,
        pass_seed: u128,
        external_object: &'a str,
    }

    fn attached_target_link(
        target: &ReviewTarget,
        fixture: AttachedTargetLinkFixture<'_>,
    ) -> ReviewExternalLink {
        let link = link_id(fixture.link_seed);
        let pass = ReviewPassEvidence::new(
            ReviewPassRef::new(
                ReviewRunRef::new(target.id(), run_id(fixture.run_seed)),
                pass_id(fixture.pass_seed),
            ),
            ReviewPassKind::ImportExternalContext,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(fixture.pass_seed + 100),
                output_frontier: frontier_id(fixture.pass_seed + 200),
                result: None,
            },
        );
        ReviewExternalLink::try_reserve(
            link,
            ReviewExternalLinkAssociation::Target(target.id()),
            target.provider().clone(),
            ReviewExternalObjectKind::Review,
            target,
        )
        .expect("fixture reservation matches its target")
        .attach(attachment_evidence(
            link,
            pass,
            key(fixture.external_object),
        ))
        .expect("fixture attachment is exact")
    }

    fn attached_finding_link(
        finding: ReviewFindingRef,
        link: ReviewExternalLinkId,
    ) -> ReviewExternalLink {
        attached_finding_link_with_pass(finding, link, succeeded_pass(20, ReviewPassKind::Publish))
    }

    fn pending_link(
        link: ReviewExternalLinkId,
        association: ReviewExternalLinkAssociation,
        object_kind: ReviewExternalObjectKind,
    ) -> ReviewExternalLink {
        ReviewExternalLink::try_reserve(
            link,
            association,
            key("code-host"),
            object_kind,
            &target_with_base(),
        )
        .expect("fixture reservation matches its canonical target")
    }

    fn reconstitute_link(link: &ReviewExternalLink) -> ReviewExternalLink {
        ReviewExternalLink::try_reconstitute(
            link.id,
            link.association,
            link.provider.clone(),
            link.object_kind,
            link.attachment.clone(),
            link.observations.clone(),
            link.claims.clone(),
            &target_with_base(),
        )
        .expect("fixture link reconstitutes from every durable child and claim")
    }

    fn attachment_evidence(
        link: ReviewExternalLinkId,
        pass: ReviewPassEvidence,
        external_object: ReviewKey,
    ) -> ReviewExternalLinkAttachment {
        let pass = pass_with_attachment_result(pass, link, &external_object);
        ReviewExternalLinkAttachment::new(
            link,
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            external_object,
        )
    }

    fn observation_evidence(
        link: ReviewExternalLinkId,
        ordinal: ReviewEventOrdinal,
        pass: ReviewPassEvidence,
        state: ReviewExternalObjectState,
    ) -> ReviewExternalLinkObservation {
        let pass = match pass.state() {
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                ..
            } => ReviewPassEvidence::new(
                pass.reference(),
                pass.kind(),
                pass.policy(),
                ReviewPassState::Succeeded {
                    turn: *turn,
                    output_frontier: *output_frontier,
                    result: Some(ReviewPassResult::ExternalLinkObservation(
                        ReviewExternalLinkObservationResult::new(link, ordinal, state),
                    )),
                },
            ),
            _ => pass,
        };
        ReviewExternalLinkObservation::new(
            link,
            ordinal,
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            state,
        )
    }

    fn no_change_pass(
        link: ReviewExternalLinkId,
        pass: ReviewPassEvidence,
        state: ReviewExternalObjectState,
    ) -> ReviewPassEvidence {
        let next = match pass.state() {
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                ..
            } => ReviewPassState::Succeeded {
                turn: *turn,
                output_frontier: *output_frontier,
                result: Some(ReviewPassResult::ExternalLinkNoChange(
                    ReviewExternalLinkNoChangeResult::new(link, ReviewEventOrdinal::one(), state),
                )),
            },
            other => other.clone(),
        };
        ReviewPassEvidence::new(pass.reference(), pass.kind(), pass.policy(), next)
    }

    fn publication_blocked_pass(
        link: ReviewExternalLinkId,
        pass: ReviewPassEvidence,
        reason: ReviewText,
    ) -> ReviewPassEvidence {
        let next = match pass.state() {
            ReviewPassState::Blocked { turn, .. } => ReviewPassState::Blocked {
                turn: *turn,
                result: Some(ReviewPassResult::ExternalLinkPublicationBlocked(
                    ReviewExternalLinkPublicationBlockedResult::new(link, reason),
                )),
            },
            other => other.clone(),
        };
        ReviewPassEvidence::new(pass.reference(), pass.kind(), pass.policy(), next)
    }

    fn attached_finding_link_with_pass(
        finding: ReviewFindingRef,
        link: ReviewExternalLinkId,
        pass: ReviewPassEvidence,
    ) -> ReviewExternalLink {
        pending_link(
            link,
            ReviewExternalLinkAssociation::Finding(finding),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(attachment_evidence(link, pass, key("external-comment-42")))
        .expect("fixture attachment belongs to the finding target")
    }

    fn finding_link_ref(
        finding: ReviewFindingRef,
        link: ReviewExternalLinkId,
    ) -> ReviewFindingExternalLinkRef {
        ReviewFindingExternalLinkRef::try_new(finding, &attached_finding_link(finding, link))
            .expect("fixture link is attached to the exact finding")
    }

    fn pending_finding_link_ref(
        finding: ReviewFindingRef,
        link: ReviewExternalLinkId,
    ) -> ReviewFindingPendingExternalLinkRef {
        let pending = pending_link(
            link,
            ReviewExternalLinkAssociation::Finding(finding),
            ReviewExternalObjectKind::ReviewComment,
        );
        ReviewFindingPendingExternalLinkRef::try_new(finding, &pending)
            .expect("fixture link is pending for the exact finding")
    }

    /// Builds exactly one more distinct finding than the defensive inventory
    /// budget admits.
    fn over_budget_finding_inventory() -> Vec<ReviewFindingRef> {
        (1..=(REVIEW_PRODUCED_FINDINGS_MAXIMUM as u128 + 1))
            .map(|value| ReviewFindingRef::new(pass_ref(PRODUCING_PASS_SEED), finding_id(value)))
            .collect()
    }

    #[track_caller]
    fn assert_pass_reconstitution_rejects(
        input: ReviewPassReconstitutionInput,
        expected: ReviewPassReconstitutionFailure,
    ) {
        let error = ReviewPass::try_reconstitute(input.clone())
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
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), state.turn()),
            state.clone(),
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
            &state
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

    #[derive(Clone, Copy)]
    enum ExpectedFindingTransition {
        Admitted,
        Rejected,
    }

    impl ExpectedFindingTransition {
        const fn status(self, next: ReviewFindingStatus) -> Option<ReviewFindingStatus> {
            match self {
                Self::Admitted => Some(next),
                Self::Rejected => None,
            }
        }
    }

    fn finding_transition_rows() -> Vec<FindingTransitionRow> {
        use ExpectedFindingTransition::{Admitted, Rejected};

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
                    canonical: referenced_finding(11, ReviewFindingStatus::Open),
                },
                ReviewFindingStatus::Duplicate,
            ),
            (
                "Superseded",
                ReviewFindingEventKind::Superseded {
                    successor: referenced_finding(12, ReviewFindingStatus::Open),
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
                    link: Box::new(finding_link_ref(finding_ref(10), link_id(30))),
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
                    link: Some(Box::new(pending_finding_link_ref(
                        finding_ref(10),
                        link_id(30),
                    ))),
                },
                ReviewFindingStatus::BlockedWithReason,
            ),
        ];
        let expected_admission = [
            [
                Admitted, Admitted, Admitted, Admitted, Admitted, Rejected, Rejected, Rejected,
            ],
            [
                Rejected, Rejected, Admitted, Admitted, Admitted, Admitted, Admitted, Admitted,
            ],
            [Rejected; 8],
            [Rejected; 8],
            [Rejected; 8],
            [Rejected; 8],
            [
                Rejected, Rejected, Rejected, Admitted, Admitted, Rejected, Admitted, Admitted,
            ],
            [Rejected; 8],
            [
                Rejected, Rejected, Rejected, Admitted, Admitted, Admitted, Admitted, Rejected,
            ],
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
                                finding_event(
                                    finding_ref(10),
                                    ReviewEventOrdinal::one(),
                                    blocked_pass(19, ReviewPassKind::Publish),
                                    ReviewFindingEventKind::BlockedWithReason {
                                        reason: text("pending acknowledgement"),
                                        link: Some(Box::new(pending_finding_link_ref(
                                            finding_ref(10),
                                            link_id(30),
                                        ))),
                                    },
                                )
                            });
                        let actual = finding_transition(current, event, previous.as_ref());
                        let expected = expected_admission[status_index][event_index].status(*next);
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
        let parent = ReviewTarget::try_new(
            target_id(2),
            key("other-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("base"),
            None,
            None,
        )
        .expect("standalone parent snapshot is valid");
        let error = ReviewTarget::try_new(
            target_id(1),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("head"),
            Some(key("base")),
            Some(&parent),
        )
        .expect_err("stack parent must share the child's provider and repository");
        assert_eq!(
            error,
            ReviewTargetError::ForeignParent {
                target: target_id(1)
            }
        );
    }

    /// INV-040: reconstitution preserves the child row's exact parent
    /// identity instead of adopting a compatible joined target.
    #[test]
    fn inv040_review_target_reconstitution_rejects_substituted_parent_identity() {
        let canonical_parent = ReviewTarget::try_new(
            target_id(2),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("parent-head"),
            None,
            None,
        )
        .expect("canonical parent target is valid");
        let error = ReviewTarget::try_reconstitute(
            target_id(1),
            canonical_parent.provider().clone(),
            canonical_parent.repository().clone(),
            ReviewTargetSubject::Commit,
            key("child-head"),
            Some(canonical_parent.head_revision().clone()),
            Some(target_id(3)),
            Some(&canonical_parent),
        )
        .expect_err("the joined parent must equal the parent stored on the child row");

        assert_eq!(
            error,
            ReviewTargetError::ParentIdentityMismatch {
                target: target_id(1)
            }
        );
    }

    /// INV-040: a parent edge requires an exact frozen child comparison.
    #[test]
    fn inv040_review_target_rejects_parent_without_base_revision() {
        let parent = ReviewTarget::try_new(
            target_id(2),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("base"),
            None,
            None,
        )
        .expect("standalone parent snapshot is valid");
        let error = ReviewTarget::try_new(
            target_id(1),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("head"),
            None,
            Some(&parent),
        )
        .expect_err("a parented target must freeze its comparison revision");
        assert_eq!(
            error,
            ReviewTargetError::MissingParentBase {
                target: target_id(1)
            }
        );
    }

    /// INV-040: the child's comparison revision is the canonical parent head.
    #[test]
    fn inv040_review_target_rejects_revision_disconnected_stack_parent() {
        let parent = ReviewTarget::try_new(
            target_id(2),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("parent-head"),
            None,
            None,
        )
        .expect("standalone parent snapshot is valid");
        let error = ReviewTarget::try_new(
            target_id(1),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("child-head"),
            Some(key("unrelated-base")),
            Some(&parent),
        )
        .expect_err("a stack edge must join the exact frozen revisions");
        assert_eq!(
            error,
            ReviewTargetError::DisconnectedParent {
                target: target_id(1)
            }
        );
    }

    /// INV-040: complete stack ancestry cannot repeat the target being
    /// constructed.
    #[test]
    fn inv040_review_target_rejects_transitive_parent_cycle() {
        let root = ReviewTarget::try_new(
            target_id(CYCLIC_ROOT_TARGET),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key(CYCLIC_ROOT_HEAD),
            None,
            None,
        )
        .expect("fixture root target is canonical");
        let parent = ReviewTarget::try_new(
            target_id(CYCLIC_PARENT_TARGET),
            root.provider().clone(),
            root.repository().clone(),
            ReviewTargetSubject::Commit,
            key(CYCLIC_PARENT_HEAD),
            Some(root.head_revision().clone()),
            Some(&root),
        )
        .expect("fixture parent extends the root");
        let error = ReviewTarget::try_new(
            root.id(),
            root.provider().clone(),
            root.repository().clone(),
            ReviewTargetSubject::Commit,
            key(CYCLIC_CHILD_HEAD),
            Some(parent.head_revision().clone()),
            Some(&parent),
        )
        .expect_err("the complete canonical chain cannot contain the child");

        assert_eq!(error, ReviewTargetError::CyclicParent { target: root.id() });
    }

    /// INV-040: refresh history for one change request is not an immediate
    /// stack edge.
    #[test]
    fn inv040_review_target_rejects_immediate_same_change_request_parent() {
        let parent = ReviewTarget::try_new(
            target_id(2),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::ChangeRequest(
                ReviewChangeRequestNumber::try_new(42).expect("positive change-request number"),
            ),
            key("parent-head"),
            Some(key("parent-base")),
            None,
        )
        .expect("fixture parent freezes its comparison");
        let error = ReviewTarget::try_new(
            target_id(1),
            parent.provider().clone(),
            parent.repository().clone(),
            parent.subject(),
            key("refreshed-head"),
            Some(parent.head_revision().clone()),
            Some(&parent),
        )
        .expect_err("a refreshed snapshot cannot parent the same logical change request");

        assert_eq!(
            error,
            ReviewTargetError::RepeatedChangeRequest {
                target: target_id(1)
            }
        );
    }

    /// INV-040: refresh history for one change request cannot reappear
    /// transitively in a stack chain.
    #[test]
    fn inv040_review_target_rejects_transitive_same_change_request_parent() {
        let root = ReviewTarget::try_new(
            target_id(3),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::ChangeRequest(
                ReviewChangeRequestNumber::try_new(42).expect("positive change-request number"),
            ),
            key("root-head"),
            Some(key("root-base")),
            None,
        )
        .expect("fixture root freezes its comparison");
        let parent = ReviewTarget::try_new(
            target_id(2),
            root.provider().clone(),
            root.repository().clone(),
            ReviewTargetSubject::ChangeRequest(
                ReviewChangeRequestNumber::try_new(43).expect("positive change-request number"),
            ),
            key("parent-head"),
            Some(root.head_revision().clone()),
            Some(&root),
        )
        .expect("a distinct change request extends the stack");
        let error = ReviewTarget::try_new(
            target_id(1),
            parent.provider().clone(),
            parent.repository().clone(),
            root.subject(),
            key("refreshed-head"),
            Some(parent.head_revision().clone()),
            Some(&parent),
        )
        .expect_err("a logical change request cannot repeat deeper in the stack");

        assert_eq!(
            error,
            ReviewTargetError::RepeatedChangeRequest {
                target: target_id(1)
            }
        );
    }

    /// INV-040: a valid parent reference retains canonical scope and head
    /// evidence from the supplied snapshot.
    #[test]
    fn inv040_review_target_derives_parent_evidence_from_canonical_snapshot() {
        let parent = ReviewTarget::try_new(
            target_id(2),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("parent-head"),
            None,
            None,
        )
        .expect("standalone parent snapshot is valid");
        let child = ReviewTarget::try_new(
            target_id(1),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("child-head"),
            Some(key("parent-head")),
            Some(&parent),
        )
        .expect("canonical parent head matches the child base");
        let edge = child.stack_parent().expect("child retains its parent edge");

        assert_eq!(edge.target(), parent.id());
        assert_eq!(edge.provider(), parent.provider());
        assert_eq!(edge.repository(), parent.repository());
        assert_eq!(edge.head_revision(), parent.head_revision());
        assert_eq!(child.ancestry(), &[parent.id()]);
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
            run_evidence(),
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
            produced_findings_pass(
                finding_ref(10),
                succeeded_pass(3, ReviewPassKind::ReadOnlyReview),
            ),
            run_evidence(),
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
            run_evidence(),
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
        let pass = ReviewPassEvidence::new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            ReviewPassState::Failed { turn: turn_id(6) },
        );
        let error = ReviewFindingProposal::try_new(
            finding_ref(10),
            pass.clone(),
            pass_run_evidence(&pass),
            &target,
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect_err("failed review pass cannot produce durable finding content");
        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleProducingPassEvidence
        );
    }

    /// INV-040: the finding producer's policy must come from its exact
    /// independently loaded run.
    #[test]
    fn inv040_finding_proposal_rejects_foreign_producing_run_policy() {
        let target = target_with_base();
        let later_policy = unsupported_policy();
        let error = ReviewFindingProposal::try_new(
            finding_ref(10),
            succeeded_pass(3, ReviewPassKind::ReadOnlyReview),
            ReviewRunEvidence::new(
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                later_policy,
                ReviewRunState::Succeeded {
                    concluding_pass: pass_ref(PRODUCING_PASS_SEED),
                },
            ),
            &target,
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect_err("finding policy must be authenticated by its producing run");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleProducingRunEvidence
        );
    }

    /// INV-040: a finding cannot be produced while its canonical run remains
    /// active.
    #[test]
    fn inv040_finding_proposal_rejects_contradictory_run_lifecycle() {
        let target = target_with_base();
        let pass = produced_findings_pass(
            finding_ref(10),
            succeeded_pass(PRODUCING_PASS_SEED, ReviewPassKind::ReadOnlyReview),
        );
        let run = ReviewRunEvidence::new(
            pass.reference().run(),
            ReviewWorkflowKind::ReadOnlyReview,
            pass.policy(),
            ReviewRunState::Running {
                active_pass: pass.reference(),
            },
        );
        let error = ReviewFindingProposal::try_new(
            finding_ref(10),
            pass,
            run,
            &target,
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect_err("a succeeded pass contradicts a still-running canonical run");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleProducingRunEvidence
        );
    }

    /// INV-040: a producing run must name the exact pass that produced the
    /// finding inventory.
    #[test]
    fn inv040_finding_proposal_rejects_run_naming_another_pass() {
        let target = target_with_base();
        let pass = produced_findings_pass(
            finding_ref(10),
            succeeded_pass(PRODUCING_PASS_SEED, ReviewPassKind::ReadOnlyReview),
        );
        let run = ReviewRunEvidence::new(
            pass.reference().run(),
            ReviewWorkflowKind::ReadOnlyReview,
            pass.policy(),
            ReviewRunState::Succeeded {
                concluding_pass: ReviewPassRef::new(pass.reference().run(), pass_id(99)),
            },
        );
        let error = ReviewFindingProposal::try_new(
            finding_ref(10),
            pass,
            run,
            &target,
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect_err("the canonical run must conclude with the producing pass");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleProducingRunEvidence
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
                    ReviewPolicy::version_one(),
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
                    ReviewPolicy::version_one(),
                    ReviewPassState::Running { turn: turn_id(6) },
                )),
            )
            .expect_err("publication pass cannot execute a read-only-review run");
        assert_eq!(
            error.failure(),
            ReviewRunTransitionFailure::Evidence(ReviewRunEvidenceFailure::PassKindMismatch)
        );
    }

    /// INV-040: a transition cannot introduce a pass that was never admitted
    /// through queued-pass construction.
    #[test]
    fn inv040_run_transition_rejects_unrecorded_pass() {
        let queued = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let pass = ReviewPassEvidence::new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            ReviewPassState::Running { turn: turn_id(6) },
        );
        let error = queued
            .transition(
                ReviewRunState::Running {
                    active_pass: pass.reference(),
                },
                Some(pass),
            )
            .expect_err("run transition cannot admit a previously unrecorded pass");

        assert_eq!(
            error.failure(),
            ReviewRunTransitionFailure::Evidence(ReviewRunEvidenceFailure::PassMismatch)
        );
    }

    /// INV-040: queued cancellation retains an already-recorded pass and its
    /// canonical pre-start cancellation.
    #[test]
    fn inv040_queued_run_cancellation_retains_cancelled_pass() {
        let pass = pass_ref(3);
        let next = ReviewRunState::Cancelled {
            last_pass: Some(pass),
        };
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        ReviewPass::try_new(
            pass,
            ReviewPassKind::ReadOnlyReview,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
        )
        .expect("queued-pass construction records the pass");
        let cancelled = run
            .transition(
                next,
                Some(ReviewPassEvidence::new(
                    pass,
                    ReviewPassKind::ReadOnlyReview,
                    ReviewPolicy::version_one(),
                    ReviewPassState::Cancelled { turn: None },
                )),
            )
            .expect("pre-start cancellation retains its recorded pass");

        assert_eq!(cancelled.state(), next);
    }

    /// INV-040: cancellation after activation must retain the exact turn that
    /// was cancelled.
    #[test]
    fn inv040_running_run_rejects_turnless_pass_cancellation() {
        let pass = pass_ref(3);
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        ReviewPass::try_new(
            pass,
            ReviewPassKind::ReadOnlyReview,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
        )
        .expect("queued-pass construction records the pass");
        let running = run
            .transition(
                ReviewRunState::Running { active_pass: pass },
                Some(ReviewPassEvidence::new(
                    pass,
                    ReviewPassKind::ReadOnlyReview,
                    ReviewPolicy::version_one(),
                    ReviewPassState::Running { turn: turn_id(6) },
                )),
            )
            .expect("queued run may activate its canonical pass");
        let error = running
            .transition(
                ReviewRunState::Cancelled {
                    last_pass: Some(pass),
                },
                Some(ReviewPassEvidence::new(
                    pass,
                    ReviewPassKind::ReadOnlyReview,
                    ReviewPolicy::version_one(),
                    ReviewPassState::Cancelled { turn: None },
                )),
            )
            .expect_err("running cancellation cannot erase activation evidence");

        assert_eq!(
            error.failure(),
            ReviewRunTransitionFailure::InvalidTransition
        );
    }

    /// INV-040: pass construction records its identity on the run, so later
    /// cancellation cannot claim that no pass existed.
    #[test]
    fn inv040_queued_run_rejects_passless_cancellation_after_pass_construction() {
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
        )
        .expect("queued pass belongs to its canonical run");
        assert_eq!(run.recorded_pass(), Some(pass_ref(3)));

        let error = run
            .transition(ReviewRunState::Cancelled { last_pass: None }, None)
            .expect_err("recorded queued pass cannot be discarded by cancellation");
        assert_eq!(
            error.failure(),
            ReviewRunTransitionFailure::Evidence(ReviewRunEvidenceFailure::PassMismatch)
        );
    }

    /// INV-040: queued pass construction authenticates its accepted-input
    /// session.
    #[test]
    fn inv040_pass_construction_rejects_foreign_accepted_input_session() {
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let cross_wired_input = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(99),
                Some(turn_id(6)),
            ),
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
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let error = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::Publish,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
        )
        .expect_err("pass kind must correspond to its canonical run workflow");
        assert_eq!(
            error.failure(),
            ReviewPassConstructionFailure::RunWorkflowMismatch
        );
    }

    /// INV-040: queued-pass construction takes workflow evidence only from the
    /// pass's exact parent run.
    #[test]
    fn inv040_pass_construction_rejects_foreign_run_evidence() {
        let mut foreign_run = ReviewRun::new(
            ReviewRunRef::new(target_id(1), run_id(99)),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let error = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            &mut foreign_run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
        )
        .expect_err("workflow evidence must come from the pass's exact run");

        assert_eq!(error.failure(), ReviewPassConstructionFailure::ForeignRun);
        assert_eq!(
            error.run_evidence().reference(),
            ReviewRunRef::new(target_id(1), run_id(99))
        );
    }

    /// INV-040: one active pass turn remains fixed through terminalization.
    #[test]
    fn inv040_pass_transition_rejects_changed_turn() {
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let running = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
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
                    result: None,
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
        let lagging = ReviewPassState::Running { turn: turn_id(6) };
        let input = ReviewPassReconstitutionInput::new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
            lagging.clone(),
            Some(ReviewPassTurnEvidence::new(
                turn_id(6),
                session_id(4),
                accepted_input_id(5),
                ReviewPassTurnOutcome::Completed,
                Some(frontier_id(8)),
            )),
        );
        assert_eq!(
            ReviewPass::try_reconstitute(input)
                .expect("a running pass may lag its terminal canonical turn")
                .state(),
            &lagging
        );
    }

    /// S29 / INV-040: a queued pass starts only while its canonical turn is
    /// active; terminal outcomes cannot lead an unprojected start.
    #[test]
    fn s29_inv040_queued_pass_start_requires_active_turn() {
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let queued = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
        )
        .expect("accepted input belongs to the pass session");
        let error = queued
            .transition(
                ReviewPassState::Running { turn: turn_id(6) },
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            )
            .expect_err("a finished turn cannot lead an unprojected start");
        assert_eq!(error.failure(), ReviewPassTransitionFailure::TurnNotActive);
    }

    /// S29 / INV-040: run reconstitution accepts its exact canonical pass
    /// outcome.
    #[test]
    fn s29_inv040_run_reconstitution_accepts_exact_pass_outcome() {
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
                ReviewPolicy::version_one(),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
            )),
        );
        assert_eq!(
            ReviewRun::try_reconstitute(exact)
                .expect("canonical pass outcome supports the run")
                .state(),
            state
        );
    }

    /// S29 / INV-040: run reconstitution rejects a contradictory canonical
    /// pass outcome.
    #[test]
    fn s29_inv040_run_reconstitution_rejects_cross_wired_pass_outcome() {
        let state = ReviewRunState::Succeeded {
            concluding_pass: pass_ref(3),
        };
        let mismatched = ReviewRunReconstitutionInput::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            state,
            Some(ReviewPassEvidence::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewPolicy::version_one(),
                ReviewPassState::Failed { turn: turn_id(6) },
            )),
        );
        let mismatch = ReviewRun::try_reconstitute(mismatched.clone())
            .expect_err("a failed pass cannot support a succeeded run");
        assert_eq!(
            mismatch.failure(),
            ReviewRunEvidenceFailure::PassStateMismatch
        );
        assert_eq!(mismatch.input(), &mismatched);
    }

    /// S29 / INV-040: canonical pass evidence must carry the run's frozen
    /// policy.
    #[test]
    fn s29_inv040_run_reconstitution_rejects_foreign_pass_policy() {
        let state = ReviewRunState::Succeeded {
            concluding_pass: pass_ref(3),
        };
        let foreign_policy = unsupported_policy();
        let mismatched = ReviewRunReconstitutionInput::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            state,
            Some(ReviewPassEvidence::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                foreign_policy,
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
            )),
        );
        let error = ReviewRun::try_reconstitute(mismatched.clone())
            .expect_err("pass evidence under another policy cannot support the run");
        assert_eq!(
            error.failure(),
            ReviewRunEvidenceFailure::PassPolicyMismatch
        );
        assert_eq!(error.input(), &mismatched);
    }

    /// INV-040: the workflow discriminator must come from the pass's own run
    /// row.
    #[test]
    fn inv040_pass_reconstitution_rejects_foreign_workflow_run() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                ReviewRunRef::new(target_id(1), run_id(9)),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            ),
            ReviewPassReconstitutionFailure::ForeignWorkflowRun,
        );
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
        let missing_error = ReviewRun::try_reconstitute(missing.clone())
            .expect_err("a concluding run requires canonical pass evidence");
        assert_eq!(
            missing_error.failure(),
            ReviewRunEvidenceFailure::MissingPassEvidence
        );
        assert_eq!(missing_error.input(), &missing);
    }

    /// INV-040: exact canonical accepted-input, turn, and frontier evidence
    /// reconstitutes the stored pass state.
    #[test]
    fn inv040_pass_reconstitution_accepts_exact_canonical_evidence() {
        let state = ReviewPassState::Succeeded {
            turn: turn_id(6),
            output_frontier: frontier_id(8),
            result: Some(ReviewPassResult::ProducedFindings(
                ReviewProducedFindings::try_new(Vec::new())
                    .expect("an empty review inventory is complete"),
            )),
        };
        let exact = ReviewPassReconstitutionInput::new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            session_id(4),
            accepted_input_id(5),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
            state.clone(),
            Some(ReviewPassTurnEvidence::new(
                turn_id(6),
                session_id(4),
                accepted_input_id(5),
                ReviewPassTurnOutcome::Completed,
                Some(frontier_id(8)),
            )),
        );
        assert_eq!(
            ReviewPass::try_reconstitute(exact)
                .expect("all canonical evidence matches")
                .state(),
            &state
        );
    }

    /// INV-040: pass reconstitution preserves the pass row's accepted-input
    /// identity instead of adopting the joined evidence identity.
    #[test]
    fn inv040_pass_reconstitution_rejects_substituted_accepted_input_identity() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(9),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Queued,
                None,
            ),
            ReviewPassReconstitutionFailure::AcceptedInputEvidenceMismatch,
        );
    }

    /// INV-040: the accepted input must belong to the pass session.
    #[test]
    fn inv040_pass_reconstitution_rejects_foreign_accepted_input_session() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(9),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            ),
            ReviewPassReconstitutionFailure::AcceptedInputSessionMismatch,
        );
    }

    /// INV-040: a turn-naming pass state requires its canonical turn row.
    #[test]
    fn inv040_pass_reconstitution_rejects_missing_turn_evidence() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
                None,
            ),
            ReviewPassReconstitutionFailure::MissingTurnEvidence,
        );
    }

    /// INV-040: a queued pass admits no turn evidence.
    #[test]
    fn inv040_pass_reconstitution_rejects_unexpected_turn_evidence() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Queued,
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            ),
            ReviewPassReconstitutionFailure::UnexpectedTurnEvidence,
        );
    }

    /// INV-040: the canonical turn row must name the pass state's exact turn.
    #[test]
    fn inv040_pass_reconstitution_rejects_foreign_turn() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
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
    }

    /// INV-040: the canonical turn must belong to the pass session.
    #[test]
    fn inv040_pass_reconstitution_rejects_foreign_turn_session() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
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
    }

    /// INV-040: the canonical turn must originate from the pass input.
    #[test]
    fn inv040_pass_reconstitution_rejects_foreign_turn_origin_input() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
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
    }

    /// INV-040: a succeeded pass rejects a contradictory canonical outcome.
    #[test]
    fn inv040_pass_reconstitution_rejects_mismatched_turn_outcome() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
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
    }

    /// INV-040: the successful output must be the canonical terminal frontier.
    #[test]
    fn inv040_pass_reconstitution_rejects_mismatched_output_frontier() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
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
    }

    /// S29 / INV-040: a running pass accepts an active canonical turn.
    #[test]
    fn s29_inv040_running_pass_accepts_active_turn_outcome() {
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Running { turn: turn_id(6) },
            ReviewPassTurnOutcome::Active,
            None,
        );
    }

    /// S29 / INV-040: a failed pass may project completed execution whose
    /// workflow result was invalid.
    #[test]
    fn s29_inv040_failed_pass_accepts_completed_turn_outcome() {
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Failed { turn: turn_id(6) },
            ReviewPassTurnOutcome::Completed,
            Some(frontier_id(8)),
        );
    }

    /// S29 / INV-040: a failed pass accepts a failed canonical turn.
    #[test]
    fn s29_inv040_failed_pass_accepts_failed_turn_outcome() {
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Failed { turn: turn_id(6) },
            ReviewPassTurnOutcome::Failed,
            Some(frontier_id(8)),
        );
    }

    /// S29 / INV-040: a failed pass accepts a refused canonical turn.
    #[test]
    fn s29_inv040_failed_pass_accepts_refused_turn_outcome() {
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Failed { turn: turn_id(6) },
            ReviewPassTurnOutcome::Refused,
            Some(frontier_id(8)),
        );
    }

    /// S29 / INV-040: a blocked pass accepts a reconciliation-required turn.
    #[test]
    fn s29_inv040_blocked_pass_accepts_reconciliation_turn_outcome() {
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Blocked {
                turn: turn_id(6),
                result: None,
            },
            ReviewPassTurnOutcome::ReconciliationRequired,
            Some(frontier_id(8)),
        );
    }

    /// S29 / INV-040: a post-start cancelled pass accepts a cancelled turn.
    #[test]
    fn s29_inv040_cancelled_pass_accepts_cancelled_turn_outcome() {
        assert_pass_outcome_reconstitutes(
            ReviewPassState::Cancelled {
                turn: Some(turn_id(6)),
            },
            ReviewPassTurnOutcome::Cancelled,
            Some(frontier_id(8)),
        );
    }

    /// S29 / INV-040: authenticated successful pass evidence may project one
    /// exact effect result without changing its execution facts.
    #[test]
    fn inv040_succeeded_pass_evidence_projects_one_exact_effect_result() {
        let pass = succeeded_pass(70, ReviewPassKind::ImportExternalContext);
        let result = ReviewPassResult::ExternalLinkNoChange(ReviewExternalLinkNoChangeResult::new(
            link_id(71),
            ReviewEventOrdinal::one(),
            ReviewExternalObjectState::Current,
        ));

        let projected = pass
            .project_result(result.clone())
            .expect("succeeded evidence may project an atomic effect result");
        let ReviewPassState::Succeeded {
            turn,
            output_frontier,
            ..
        } = pass.state()
        else {
            panic!("fixture is a succeeded pass");
        };

        assert_eq!(projected.reference(), pass.reference());
        assert_eq!(projected.kind(), pass.kind());
        assert_eq!(projected.policy(), pass.policy());
        assert_eq!(
            projected.state(),
            &ReviewPassState::Succeeded {
                turn: *turn,
                output_frontier: *output_frontier,
                result: Some(result),
            }
        );
    }

    /// S29 / INV-040: an authenticated effect projection admits exact replay
    /// but cannot be rebound to a distinct result.
    #[test]
    fn inv040_pass_evidence_result_projection_is_immutable() {
        let pass = succeeded_pass(71, ReviewPassKind::ImportExternalContext);
        let result = ReviewPassResult::ExternalLinkNoChange(ReviewExternalLinkNoChangeResult::new(
            link_id(72),
            ReviewEventOrdinal::one(),
            ReviewExternalObjectState::Current,
        ));
        let projected = pass
            .project_result(result.clone())
            .expect("result-free succeeded evidence admits its first result");
        let distinct =
            ReviewPassResult::ExternalLinkNoChange(ReviewExternalLinkNoChangeResult::new(
                link_id(72),
                ReviewEventOrdinal::one(),
                ReviewExternalObjectState::Outdated,
            ));

        assert_eq!(projected.project_result(result), Some(projected.clone()));
        assert_eq!(projected.project_result(distinct), None);
    }

    /// S29 / INV-040: authenticated blocked pass evidence may project one exact
    /// effect result without changing its execution facts.
    #[test]
    fn inv040_blocked_pass_evidence_projects_one_exact_effect_result() {
        let pass = ReviewPassEvidence::new(
            pass_ref(72),
            ReviewPassKind::Publish,
            ReviewPolicy::version_one(),
            ReviewPassState::Blocked {
                turn: turn_id(73),
                result: None,
            },
        );
        let result = ReviewPassResult::ExternalLinkPublicationBlocked(
            ReviewExternalLinkPublicationBlockedResult::new(
                link_id(74),
                text("publication is unavailable"),
            ),
        );

        let projected = pass
            .project_result(result.clone())
            .expect("blocked evidence may project an atomic effect result");

        assert_eq!(projected.reference(), pass.reference());
        assert_eq!(projected.kind(), pass.kind());
        assert_eq!(projected.policy(), pass.policy());
        assert_eq!(
            projected.state(),
            &ReviewPassState::Blocked {
                turn: turn_id(73),
                result: Some(result),
            }
        );
    }

    /// S29 / INV-040: non-effect pass states cannot project effect results.
    #[test]
    fn inv040_non_effect_pass_evidence_rejects_result_projection() {
        let result = ReviewPassResult::ExternalLinkNoChange(ReviewExternalLinkNoChangeResult::new(
            link_id(75),
            ReviewEventOrdinal::one(),
            ReviewExternalObjectState::Current,
        ));
        let queued = ReviewPassEvidence::new(
            pass_ref(76),
            ReviewPassKind::ImportExternalContext,
            ReviewPolicy::version_one(),
            ReviewPassState::Queued,
        );
        let running = ReviewPassEvidence::new(
            pass_ref(77),
            ReviewPassKind::ImportExternalContext,
            ReviewPolicy::version_one(),
            ReviewPassState::Running { turn: turn_id(78) },
        );
        let failed = ReviewPassEvidence::new(
            pass_ref(79),
            ReviewPassKind::ImportExternalContext,
            ReviewPolicy::version_one(),
            ReviewPassState::Failed { turn: turn_id(80) },
        );
        let cancelled = ReviewPassEvidence::new(
            pass_ref(81),
            ReviewPassKind::ImportExternalContext,
            ReviewPolicy::version_one(),
            ReviewPassState::Cancelled {
                turn: Some(turn_id(82)),
            },
        );

        assert_eq!(queued.project_result(result.clone()), None);
        assert_eq!(running.project_result(result.clone()), None);
        assert_eq!(failed.project_result(result.clone()), None);
        assert_eq!(cancelled.project_result(result), None);
    }

    /// S29 / INV-040: a terminal canonical turn outcome always carries its
    /// checked terminal frontier.
    #[test]
    fn s29_inv040_pass_evidence_rejects_terminal_outcome_without_frontier() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Running { turn: turn_id(6) },
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    None,
                )),
            ),
            ReviewPassReconstitutionFailure::TurnFrontierShapeMismatch,
        );
    }

    /// S29 / INV-040: an active canonical turn outcome never carries a
    /// terminal frontier.
    #[test]
    fn s29_inv040_pass_evidence_rejects_active_outcome_with_frontier() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Running { turn: turn_id(6) },
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Active,
                    Some(frontier_id(8)),
                )),
            ),
            ReviewPassReconstitutionFailure::TurnFrontierShapeMismatch,
        );
    }

    /// INV-040 / INV-041: a pending reservation is not posting evidence.
    #[test]
    fn inv040_posted_link_rejects_pending_reservation() {
        let finding = finding_ref(10);
        let pending = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding),
            ReviewExternalObjectKind::ReviewComment,
        );

        let error = ReviewFindingExternalLinkRef::try_new(finding, &pending)
            .expect_err("pending reservation is not posting evidence");

        assert_eq!(
            error.failure(),
            ReviewFindingExternalLinkFailure::NotAttached
        );
    }

    /// INV-040 / INV-041: a publication block binds one exact pending
    /// reservation associated with the finding.
    #[test]
    fn inv040_blocked_link_accepts_exact_pending_reservation() {
        let finding = finding_ref(10);
        let pending = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding),
            ReviewExternalObjectKind::ReviewComment,
        );

        let blocked = ReviewFindingPendingExternalLinkRef::try_new(finding, &pending)
            .expect("pending finding association supports a publication block");

        assert_eq!(blocked.finding(), finding);
        assert_eq!(blocked.link(), pending.id());
    }

    /// INV-040 / INV-041: an already attached object cannot be recorded as a
    /// pending publication attempt.
    #[test]
    fn inv040_blocked_link_rejects_attached_reservation() {
        let finding = finding_ref(10);
        let attached = attached_finding_link(finding, link_id(30));

        let error = ReviewFindingPendingExternalLinkRef::try_new(finding, &attached)
            .expect_err("attached reservation is no longer pending");

        assert_eq!(
            error.failure(),
            ReviewFindingExternalLinkFailure::AlreadyAttached
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

    /// INV-040 / INV-041: a repository correlation that carries no review
    /// content cannot prove that a finding was posted.
    #[test]
    fn inv040_posted_link_rejects_non_review_external_object() {
        let finding = finding_ref(10);
        let link = pending_link(
            link_id(32),
            ReviewExternalLinkAssociation::Finding(finding),
            ReviewExternalObjectKind::Commit,
        )
        .attach(attachment_evidence(
            link_id(32),
            succeeded_pass(20, ReviewPassKind::Publish),
            key("external-commit"),
        ))
        .expect("fixture attachment belongs to the finding target");

        let error = ReviewFindingExternalLinkRef::try_new(finding, &link)
            .expect_err("an attached commit does not prove review publication");

        assert_eq!(
            error.failure(),
            ReviewFindingExternalLinkFailure::IncompatibleObjectKind
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
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::one(),
                succeeded_pass(19, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ))
            .expect("open finding may be accepted")
            .apply(posted_finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                succeeded_pass(20, ReviewPassKind::Publish),
                link_id(30),
            ))
            .expect("accepted finding may be posted")
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(3).expect("positive ordinal"),
                succeeded_pass(22, ReviewPassKind::Fix),
                ReviewFindingEventKind::Fixed,
            ))
            .expect("posted finding may be fixed");
        assert_eq!(finding.status(), ReviewFindingStatus::Fixed);

        let reopened = finding
            .apply(finding_event(
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
            .apply(finding_event(
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
        let event = finding_event(
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

    /// INV-040: finding-event reconstitution preserves the event row's exact
    /// pass identity instead of adopting compatible joined pass evidence.
    #[test]
    fn inv040_finding_history_rejects_substituted_event_pass_identity() {
        let finding = finding_ref(10);
        let ordinal = ReviewEventOrdinal::one();
        let pass = pass_with_finding_event(
            finding,
            ordinal,
            succeeded_pass(20, ReviewPassKind::Judge),
            &ReviewFindingEventKind::Accepted,
        );
        let event = ReviewFindingEvent::new(
            finding,
            ordinal,
            pass_ref(21),
            pass.clone(),
            pass_run_evidence(&pass),
            ReviewFindingEventKind::Accepted,
        );
        let error = ReviewFinding::new(proposal())
            .apply(event)
            .expect_err("canonical pass evidence must match the pass stored on the event");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::EventPassEvidenceMismatch
        );
    }

    /// INV-040: a referenced finding naming the aggregate's own identity is a
    /// self-reference even when its producing-pass ancestry is cross-wired.
    #[test]
    fn inv040_finding_history_rejects_identity_self_reference() {
        let event = finding_event(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            succeeded_pass(20, ReviewPassKind::Dedupe),
            ReviewFindingEventKind::Duplicate {
                canonical: ReviewReferencedFindingEvidence {
                    reference: ReviewFindingRef::new(pass_ref(4), finding_id(10)),
                    status: ReviewFindingStatus::Open,
                    producer_policy: ReviewPolicy::version_one(),
                },
            },
        );
        let error = ReviewFinding::new(proposal())
            .apply(event.clone())
            .expect_err("a finding cannot be its own canonical duplicate");
        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::SelfReference
        );
        assert_eq!(error.event(), Some(&event));
    }

    /// INV-040: a same-target pass cannot produce an event outside its closed
    /// pass-kind responsibility.
    #[test]
    fn inv040_finding_history_rejects_incompatible_event_pass_kind() {
        let event = finding_event(
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

    /// INV-040: every event pass uses the exact policy frozen by the finding's
    /// producing run.
    #[test]
    fn inv040_finding_history_rejects_event_policy_mismatch() {
        let later_policy = unsupported_policy();
        let event = finding_event(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            ReviewPassEvidence::new(
                pass_ref(20),
                ReviewPassKind::Judge,
                later_policy,
                ReviewPassState::Succeeded {
                    turn: turn_id(120),
                    output_frontier: frontier_id(220),
                    result: None,
                },
            ),
            ReviewFindingEventKind::Accepted,
        );
        let error = ReviewFinding::new(proposal())
            .apply(event)
            .expect_err("event policy must equal the finding policy");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::EventPolicyMismatch
        );
    }

    /// INV-040: an event pass is authenticated against its exact canonical run
    /// workflow rather than a copied compatible tuple.
    #[test]
    fn inv040_finding_history_rejects_cross_wired_event_run() {
        let finding = finding_ref(10);
        let ordinal = ReviewEventOrdinal::one();
        let pass = pass_with_finding_event(
            finding,
            ordinal,
            succeeded_pass(20, ReviewPassKind::Judge),
            &ReviewFindingEventKind::Accepted,
        );
        let run = ReviewRunEvidence::new(
            pass.reference().run(),
            ReviewWorkflowKind::PublishReview,
            pass.policy(),
            pass_run_evidence(&pass).state(),
        );
        let event = ReviewFindingEvent::new(
            finding,
            ordinal,
            pass.reference(),
            pass,
            run,
            ReviewFindingEventKind::Accepted,
        );
        let error = ReviewFinding::new(proposal())
            .apply(event)
            .expect_err("event pass workflow must come from its owning run");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleEventRunEvidence
        );
    }

    /// INV-040: a successful finding event pass cannot be paired with a failed
    /// canonical run projection.
    #[test]
    fn inv040_finding_history_rejects_event_run_lifecycle_mismatch() {
        let finding = finding_ref(10);
        let ordinal = ReviewEventOrdinal::one();
        let pass = pass_with_finding_event(
            finding,
            ordinal,
            succeeded_pass(20, ReviewPassKind::Judge),
            &ReviewFindingEventKind::Accepted,
        );
        let run = ReviewRunEvidence::new(
            pass.reference().run(),
            ReviewWorkflowKind::JudgeFindings,
            pass.policy(),
            ReviewRunState::Failed {
                failed_pass: pass.reference(),
            },
        );
        let event = ReviewFindingEvent::new(
            finding,
            ordinal,
            pass.reference(),
            pass,
            run,
            ReviewFindingEventKind::Accepted,
        );
        let error = ReviewFinding::new(proposal())
            .apply(event)
            .expect_err("a succeeded event pass contradicts a failed canonical run");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleEventRunEvidence
        );
    }

    /// INV-040: a pass result must name the event's exact finding, ordinal, and
    /// discriminator.
    #[test]
    fn inv040_finding_history_rejects_mismatched_pass_result() {
        let finding = finding_ref(10);
        let pass = pass_with_finding_event(
            finding,
            ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
            succeeded_pass(20, ReviewPassKind::Judge),
            &ReviewFindingEventKind::Accepted,
        );
        let event = ReviewFindingEvent::new(
            finding,
            ReviewEventOrdinal::one(),
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            ReviewFindingEventKind::Accepted,
        );
        let error = ReviewFinding::new(proposal())
            .apply(event)
            .expect_err("another event ordinal cannot reuse the pass result");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleEventPassEvidence
        );
    }

    /// INV-040: canonical and successor references admit only findings that do
    /// not already carry a terminal reference edge.
    #[test]
    fn inv040_finding_history_rejects_ineligible_reference() {
        let event = finding_event(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            succeeded_pass(20, ReviewPassKind::Dedupe),
            ReviewFindingEventKind::Duplicate {
                canonical: referenced_finding(11, ReviewFindingStatus::Duplicate),
            },
        );
        let error = ReviewFinding::new(proposal())
            .apply(event)
            .expect_err("a duplicate cannot become another canonical reference");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IneligibleReferencedFinding
        );
    }

    /// INV-040: reconstitution preserves the status frozen when a reference
    /// was admitted, independently of the referenced finding's current state.
    #[test]
    fn inv040_referenced_finding_evidence_reconstitutes_only_admissible_status() {
        let reference = finding_ref(11);
        let producing_pass = produced_findings_pass(
            reference,
            succeeded_pass(PRODUCING_PASS_SEED, ReviewPassKind::ReadOnlyReview),
        );
        let producing_run = pass_run_evidence(&producing_pass);
        let frozen = ReviewReferencedFindingEvidence::try_reconstitute(
            reference,
            ReviewFindingStatus::Accepted,
            &producing_pass,
            producing_run,
        )
        .expect("an accepted finding is admissible reference evidence");

        assert_eq!(frozen.reference(), reference);
        assert_eq!(frozen.status(), ReviewFindingStatus::Accepted);
        assert_eq!(frozen.producer_policy(), producing_pass.policy());
        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                reference,
                ReviewFindingStatus::Posted,
                &producing_pass,
                producing_run,
            )
            .is_none(),
            "a terminal current status cannot be substituted for frozen evidence",
        );
    }

    /// INV-040: one canonical pass identity cannot change outcome evidence
    /// between events in the same complete finding history.
    #[test]
    fn inv040_finding_history_rejects_conflicting_reused_pass_evidence() {
        let finding = ReviewFinding::new(proposal())
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::one(),
                succeeded_pass(20, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ))
            .expect("successful judgment may accept a finding");
        let event = finding_event(
            finding_ref(10),
            ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
            ReviewPassEvidence::new(
                pass_ref(20),
                ReviewPassKind::Judge,
                ReviewPolicy::version_one(),
                ReviewPassState::Succeeded {
                    turn: turn_id(120),
                    output_frontier: frontier_id(999),
                    result: None,
                },
            ),
            ReviewFindingEventKind::Stale,
        );
        let error = finding
            .apply(event.clone())
            .expect_err("one pass identity cannot carry contradictory terminal evidence");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::ConflictingPassEvidence
        );
        assert_eq!(error.event(), Some(&event));
    }

    /// INV-040: a user-global pass identity cannot move to another run inside
    /// one finding history.
    #[test]
    fn inv040_finding_history_rejects_reparented_pass_identity() {
        let accepted = finding_event(
            finding_ref(CANONICAL_FINDING_SEED),
            ReviewEventOrdinal::one(),
            succeeded_pass(ARBITRARY_JUDGMENT_PASS_SEED, ReviewPassKind::Judge),
            ReviewFindingEventKind::Accepted,
        );
        let pass = ReviewPassEvidence::new(
            ReviewPassRef::new(pass_ref(REASSIGNED_PASS_SEED).run(), accepted.pass().pass()),
            ReviewPassKind::Publish,
            ReviewPolicy::version_one(),
            ReviewPassState::Blocked {
                turn: turn_id(CROSS_WIRED_TURN_SEED),
                result: None,
            },
        );
        let finding = ReviewFinding::new(proposal())
            .apply(accepted)
            .expect("canonical judgment accepts the finding");
        let error = finding
            .apply(finding_event(
                finding_ref(CANONICAL_FINDING_SEED),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                pass,
                ReviewFindingEventKind::BlockedWithReason {
                    reason: text("publication blocked"),
                    link: Some(Box::new(pending_finding_link_ref(
                        finding_ref(CANONICAL_FINDING_SEED),
                        link_id(30),
                    ))),
                },
            ))
            .expect_err("one pass identity cannot move between runs");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::ConflictingPassEvidence
        );
    }

    /// INV-040: a user-global run identity cannot change its one pass or
    /// workflow inside one finding history.
    #[test]
    fn inv040_finding_history_rejects_changed_run_claim() {
        let accepted = finding_event(
            finding_ref(CANONICAL_FINDING_SEED),
            ReviewEventOrdinal::one(),
            succeeded_pass(ARBITRARY_JUDGMENT_PASS_SEED, ReviewPassKind::Judge),
            ReviewFindingEventKind::Accepted,
        );
        let pass = ReviewPassEvidence::new(
            ReviewPassRef::new(accepted.pass().run(), pass_id(REASSIGNED_PASS_SEED)),
            ReviewPassKind::Publish,
            ReviewPolicy::version_one(),
            ReviewPassState::Blocked {
                turn: turn_id(CROSS_WIRED_TURN_SEED),
                result: None,
            },
        );
        let finding = ReviewFinding::new(proposal())
            .apply(accepted)
            .expect("canonical judgment accepts the finding");
        let error = finding
            .apply(finding_event(
                finding_ref(CANONICAL_FINDING_SEED),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                pass,
                ReviewFindingEventKind::BlockedWithReason {
                    reason: text("publication blocked"),
                    link: Some(Box::new(pending_finding_link_ref(
                        finding_ref(CANONICAL_FINDING_SEED),
                        link_id(30),
                    ))),
                },
            ))
            .expect_err("one run identity cannot change pass or workflow");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::ConflictingRunEvidence
        );
    }

    /// INV-040: complete history replay maintains indexed pass, run, and
    /// publication claims instead of rescanning the growing event vector.
    #[test]
    fn inv040_finding_history_indexes_replay_claims() {
        let finding = ReviewFinding::new(proposal())
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::one(),
                succeeded_pass(19, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ))
            .expect("finding may be accepted")
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                blocked_pass(20, ReviewPassKind::Publish),
                ReviewFindingEventKind::BlockedWithReason {
                    reason: text("lost acknowledgement"),
                    link: Some(Box::new(pending_finding_link_ref(
                        finding_ref(10),
                        link_id(31),
                    ))),
                },
            ))
            .expect("publication block retains its claim")
            .apply(posted_finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(3).expect("positive ordinal"),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                link_id(31),
            ))
            .expect("confirmed attachment reconciles the publication");

        assert_eq!(finding.pass_claims.len(), 3);
        assert_eq!(finding.run_claims.len(), 3);
        assert_eq!(finding.publication_links, BTreeSet::from([link_id(31)]));
    }

    /// INV-040: a compatible pass kind cannot author a finding event after a
    /// failed outcome.
    #[test]
    fn inv040_finding_history_rejects_incompatible_event_pass_outcome() {
        let event = finding_event(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            ReviewPassEvidence::new(
                pass_ref(20),
                ReviewPassKind::Judge,
                ReviewPolicy::version_one(),
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
        let finding = finding_ref(10);
        let ordinal = ReviewEventOrdinal::try_new(2).expect("positive ordinal");
        let result_link = finding_link_ref(finding, link_id(30));
        let pass = pass_with_finding_event(
            finding,
            ordinal,
            pass_with_attachment_result(
                succeeded_pass(21, ReviewPassKind::Publish),
                link_id(30),
                &key("external-comment-42"),
            ),
            &ReviewFindingEventKind::Posted {
                link: Box::new(result_link.clone()),
            },
        );
        let event = ReviewFindingEvent::new(
            finding,
            ordinal,
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            ReviewFindingEventKind::Posted {
                link: Box::new(result_link),
            },
        );
        let error = ReviewFinding::new(proposal())
            .apply(finding_event(
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

    /// INV-040 / INV-041: a no-write import pass may reconcile a
    /// publication-blocked finding after attaching the external object.
    #[test]
    fn inv040_publication_blocked_finding_can_reconcile_to_posted() {
        let finding = ReviewFinding::new(proposal())
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::one(),
                succeeded_pass(19, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ))
            .expect("finding may be accepted")
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                blocked_pass(20, ReviewPassKind::Publish),
                ReviewFindingEventKind::BlockedWithReason {
                    reason: text("lost acknowledgement"),
                    link: Some(Box::new(pending_finding_link_ref(
                        finding_ref(10),
                        link_id(31),
                    ))),
                },
            ))
            .expect("blocked publication retains its reason")
            .apply(posted_finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(3).expect("positive ordinal"),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                link_id(31),
            ))
            .expect("confirmed attachment reconciles the publication");
        assert_eq!(finding.status(), ReviewFindingStatus::Posted);
    }

    /// INV-040 / INV-041: reconciliation cannot substitute another pending
    /// reservation for the publication attempt that blocked.
    #[test]
    fn inv040_publication_reconciliation_rejects_another_reservation() {
        let finding = ReviewFinding::new(proposal())
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::one(),
                succeeded_pass(19, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ))
            .expect("finding may be accepted")
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                blocked_pass(20, ReviewPassKind::Publish),
                ReviewFindingEventKind::BlockedWithReason {
                    reason: text("lost acknowledgement"),
                    link: Some(Box::new(pending_finding_link_ref(
                        finding_ref(10),
                        link_id(31),
                    ))),
                },
            ))
            .expect("publication block binds its attempted reservation");
        let error = finding
            .apply(posted_finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(3).expect("positive ordinal"),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                link_id(32),
            ))
            .expect_err("another reservation cannot reconcile this attempt");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::InvalidTransition {
                current: ReviewFindingStatus::BlockedWithReason
            }
        );
    }

    /// INV-040 / INV-041: reconciliation cannot replay attachment evidence
    /// consumed by an earlier posted event.
    #[test]
    fn inv040_reposting_rejects_consumed_publication_link() {
        let finding = ReviewFinding::new(proposal())
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::one(),
                succeeded_pass(19, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ))
            .expect("finding may be accepted")
            .apply(posted_finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                succeeded_pass(20, ReviewPassKind::Publish),
                link_id(30),
            ))
            .expect("accepted finding may be posted")
            .apply(finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(3).expect("positive ordinal"),
                blocked_pass(21, ReviewPassKind::Publish),
                ReviewFindingEventKind::BlockedWithReason {
                    reason: text("publication state requires reconciliation"),
                    link: Some(Box::new(pending_finding_link_ref(
                        finding_ref(10),
                        link_id(31),
                    ))),
                },
            ))
            .expect("posted finding may become publication-blocked");
        let error = finding
            .apply(posted_finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(4).expect("positive ordinal"),
                succeeded_pass(22, ReviewPassKind::Publish),
                link_id(30),
            ))
            .expect_err("the first posting's attachment was already consumed");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::ReusedPublicationLink
        );
    }

    /// INV-040: the complete nine-state finding transition surface stays
    /// closed and reviewable as one table.
    #[test]
    fn inv040_finding_transition_matrix_is_closed() {
        expect![[r#"
            ┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ value                                                                                                                            │
            ├──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ FindingTransitionRow { current: "Open", permitted_events: "Accepted, Rejected, Duplicate, Superseded, Stale" }                   │
            │ FindingTransitionRow { current: "Accepted", permitted_events: "Duplicate, Superseded, Stale, Posted, Fixed, BlockedWithReason" } │
            │ FindingTransitionRow { current: "Rejected", permitted_events: "-" }                                                              │
            │ FindingTransitionRow { current: "Duplicate", permitted_events: "-" }                                                             │
            │ FindingTransitionRow { current: "Superseded", permitted_events: "-" }                                                            │
            │ FindingTransitionRow { current: "Stale", permitted_events: "-" }                                                                 │
            │ FindingTransitionRow { current: "Posted", permitted_events: "Superseded, Stale, Fixed, BlockedWithReason" }                      │
            │ FindingTransitionRow { current: "Fixed", permitted_events: "-" }                                                                 │
            │ FindingTransitionRow { current: "BlockedWithReason", permitted_events: "Superseded, Stale, Posted, Fixed" }                      │
            └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table(finding_transition_rows()));
    }

    /// INV-040: a repair-blocked finding cannot cross the publication-only
    /// reconciliation edge.
    #[test]
    fn inv040_repair_blocked_finding_rejects_posting() {
        let previous = finding_event(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            blocked_pass(20, ReviewPassKind::Fix),
            ReviewFindingEventKind::BlockedWithReason {
                reason: text("repair could not proceed"),
                link: None,
            },
        );

        assert_eq!(
            finding_transition(
                ReviewFindingStatus::BlockedWithReason,
                &ReviewFindingEventKind::Posted {
                    link: Box::new(finding_link_ref(finding_ref(10), link_id(30))),
                },
                Some(&previous),
            ),
            None
        );
    }

    /// INV-041: reservation leaves the external effect explicitly pending.
    #[test]
    fn inv041_external_link_reservation_is_pending() {
        let association = ReviewExternalLinkAssociation::Finding(finding_ref(10));
        let pending = pending_link(
            link_id(30),
            association,
            ReviewExternalObjectKind::ReviewComment,
        );
        assert!(pending.attachment().is_none());
        assert!(pending.observations().is_empty());
    }

    /// INV-041: an external-state observation cannot precede attachment.
    #[test]
    fn inv041_external_link_rejects_observation_before_attachment() {
        let pending = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            ReviewExternalObjectKind::ReviewComment,
        );
        let premature = pending
            .observe(observation_evidence(
                link_id(30),
                ReviewEventOrdinal::one(),
                succeeded_pass(20, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect_err("observation cannot prove an unattached effect");
        assert_eq!(
            premature.failure(),
            ReviewExternalLinkTransitionFailure::NotAttached
        );
    }

    /// INV-041: an attached link admits its first contiguous observation.
    #[test]
    fn inv041_attached_external_link_accepts_first_observation() {
        let attached = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(attachment_evidence(
            link_id(30),
            succeeded_pass(20, ReviewPassKind::Publish),
            key("external-comment-42"),
        ))
        .expect("same-target pass may attach the reservation")
        .observe(observation_evidence(
            link_id(30),
            ReviewEventOrdinal::one(),
            succeeded_pass(21, ReviewPassKind::ImportExternalContext),
            ReviewExternalObjectState::Current,
        ))
        .expect("attached link admits contiguous observations");
        let attachment = attached
            .attachment()
            .expect("accepted attachment remains on the aggregate");

        assert_eq!(attachment.external_object(), &key("external-comment-42"));
    }

    /// INV-041: failed or operation-incompatible passes cannot attach external
    /// effect evidence.
    #[test]
    fn inv041_external_link_rejects_incompatible_attachment_pass() {
        let pending = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            ReviewExternalObjectKind::ReviewComment,
        );
        let error = pending
            .attach(ReviewExternalLinkAttachment::new(
                link_id(30),
                succeeded_pass(20, ReviewPassKind::Judge).reference(),
                succeeded_pass(20, ReviewPassKind::Judge),
                pass_run_evidence(&succeeded_pass(20, ReviewPassKind::Judge)),
                key("external-comment-42"),
            ))
            .expect_err("judgment pass cannot produce an external attachment");
        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleAttachmentPass
        );
    }

    /// INV-040 / INV-041: attachment evidence rejects a nested non-posted
    /// finding event.
    #[test]
    fn inv040_inv041_attachment_rejects_nested_non_posted_event() {
        let finding = finding_ref(10);
        let external_object = key("external-comment-42");
        let pass = ReviewPassEvidence::new(
            pass_ref(20),
            ReviewPassKind::Publish,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(120),
                output_frontier: frontier_id(220),
                result: Some(ReviewPassResult::ExternalLinkAttachment(
                    ReviewExternalLinkAttachmentResult::new(
                        link_id(30),
                        external_object.clone(),
                        Some(ReviewFindingEventResult::new(
                            finding,
                            ReviewEventOrdinal::one(),
                            ReviewFindingEventResultKind::Accepted,
                        )),
                    ),
                )),
            },
        );
        let error = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(ReviewExternalLinkAttachment::new(
            link_id(30),
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            external_object,
        ))
        .expect_err("a publication attachment cannot carry an accepted event");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleAttachmentPass
        );
    }

    /// INV-040 / INV-041: attachment evidence rejects a nested posted event
    /// that names another reservation.
    #[test]
    fn inv040_inv041_attachment_rejects_nested_foreign_reservation() {
        let finding = finding_ref(10);
        let external_object = key("external-comment-42");
        let pass = ReviewPassEvidence::new(
            pass_ref(21),
            ReviewPassKind::Publish,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(121),
                output_frontier: frontier_id(221),
                result: Some(ReviewPassResult::ExternalLinkAttachment(
                    ReviewExternalLinkAttachmentResult::new(
                        link_id(30),
                        external_object.clone(),
                        Some(ReviewFindingEventResult::new(
                            finding,
                            ReviewEventOrdinal::one(),
                            ReviewFindingEventResultKind::Posted { link: link_id(31) },
                        )),
                    ),
                )),
            },
        );
        let error = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(ReviewExternalLinkAttachment::new(
            link_id(30),
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            external_object,
        ))
        .expect_err("a nested posted event must name the attachment reservation");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleAttachmentPass
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
                succeeded_pass(21, ReviewPassKind::Judge).reference(),
                succeeded_pass(21, ReviewPassKind::Judge),
                pass_run_evidence(&succeeded_pass(21, ReviewPassKind::Judge)),
                ReviewExternalObjectState::Current,
            ))
            .expect_err("judgment pass cannot author an external observation");
        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleObservationPass
        );
    }

    /// INV-041: unchanged polling does not create another durable observation.
    #[test]
    fn inv041_external_link_rejects_unchanged_observation() {
        let link = attached_finding_link(finding_ref(10), link_id(30))
            .observe(observation_evidence(
                link_id(30),
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect("first external state is meaning-bearing");
        let error = link
            .observe(observation_evidence(
                link_id(30),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                succeeded_pass(22, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect_err("unchanged state is not another durable observation");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::UnchangedObservation
        );
    }

    /// INV-040 / INV-041: an unchanged external report consumes its import
    /// pass without growing observation history.
    #[test]
    fn inv040_inv041_external_link_accepts_exact_no_change_result() {
        let reservation = link_id(30);
        let link = attached_finding_link(finding_ref(10), reservation)
            .observe(observation_evidence(
                reservation,
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect("first external state is meaning-bearing");
        let pass = no_change_pass(
            reservation,
            succeeded_pass(22, ReviewPassKind::ImportExternalContext),
            ReviewExternalObjectState::Current,
        );
        let confirmed = link
            .confirm_unchanged(pass.clone(), pass_run_evidence(&pass))
            .expect("the exact latest state consumes the import pass");

        assert_eq!(confirmed.observations().len(), 1);
        assert_eq!(confirmed.claims().len(), 1);
        assert_eq!(confirmed.claims()[0].pass_evidence(), &pass);
        assert_eq!(
            confirmed
                .observations()
                .last()
                .expect("fixture retains its one observation")
                .state(),
            ReviewExternalObjectState::Current
        );
        let reconstituted = reconstitute_link(&confirmed);
        assert_eq!(reconstituted, confirmed);
        let error = reconstituted
            .observe(observation_evidence(
                reservation,
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                succeeded_pass(22, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Outdated,
            ))
            .expect_err("a no-change pass cannot later claim a changed observation");
        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::ConflictingPassEvidence
        );
    }

    /// INV-040 / INV-041: reconstitution validates an unchanged claim against
    /// its recorded historical frontier after a later changed observation.
    #[test]
    fn inv040_inv041_external_link_reconstitutes_historical_no_change_frontier() {
        let reservation = link_id(30);
        let first = attached_finding_link(finding_ref(10), reservation)
            .observe(observation_evidence(
                reservation,
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect("first external state is meaning-bearing");
        let no_change = no_change_pass(
            reservation,
            succeeded_pass(22, ReviewPassKind::ImportExternalContext),
            ReviewExternalObjectState::Current,
        );
        let advanced = first
            .confirm_unchanged(no_change.clone(), pass_run_evidence(&no_change))
            .expect("the exact first frontier consumes its import pass")
            .observe(observation_evidence(
                reservation,
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                succeeded_pass(23, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Outdated,
            ))
            .expect("a later changed state advances the durable frontier");

        assert_eq!(reconstitute_link(&advanced), advanced);
    }

    /// INV-040 / INV-041: a no-change claim authenticates the exact latest
    /// observation, not an equal historical state.
    #[test]
    fn inv040_inv041_external_link_rejects_stale_no_change_frontier() {
        let reservation = link_id(30);
        let link = attached_finding_link(finding_ref(10), reservation)
            .observe(observation_evidence(
                reservation,
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect("first state is meaning-bearing")
            .observe(observation_evidence(
                reservation,
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                succeeded_pass(22, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Outdated,
            ))
            .expect("changed state advances the observation frontier");
        let pass = no_change_pass(
            reservation,
            succeeded_pass(23, ReviewPassKind::ImportExternalContext),
            ReviewExternalObjectState::Current,
        );

        let error = link
            .confirm_unchanged(pass.clone(), pass_run_evidence(&pass))
            .expect_err("a historical matching state cannot authenticate no-change");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleObservationPass
        );
    }

    /// INV-040 / INV-041: an unchanged report also retains its owning run
    /// claim against a later pass substitution.
    #[test]
    fn inv040_inv041_external_link_no_change_retains_run_claim() {
        let reservation = link_id(30);
        let link = attached_finding_link(finding_ref(10), reservation)
            .observe(observation_evidence(
                reservation,
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect("first external state is meaning-bearing");
        let no_change = no_change_pass(
            reservation,
            succeeded_pass(22, ReviewPassKind::ImportExternalContext),
            ReviewExternalObjectState::Current,
        );
        let reused_run = ReviewPassEvidence::new(
            ReviewPassRef::new(no_change.reference().run(), pass_id(23)),
            ReviewPassKind::ImportExternalContext,
            no_change.policy(),
            succeeded_pass(23, ReviewPassKind::ImportExternalContext)
                .state()
                .clone(),
        );
        let confirmed = link
            .confirm_unchanged(no_change.clone(), pass_run_evidence(&no_change))
            .expect("the exact latest state consumes the import pass");
        let error = confirmed
            .observe(observation_evidence(
                reservation,
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                reused_run,
                ReviewExternalObjectState::Outdated,
            ))
            .expect_err("one run cannot move to another pass after no-change");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::ConflictingRunEvidence
        );
    }

    /// INV-040 / INV-041: a no-change result cannot name a different
    /// reservation.
    #[test]
    fn inv040_inv041_external_link_rejects_foreign_no_change_result() {
        let reservation = link_id(30);
        let link = attached_finding_link(finding_ref(10), reservation)
            .observe(observation_evidence(
                reservation,
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect("first external state is meaning-bearing");
        let pass = no_change_pass(
            link_id(31),
            succeeded_pass(22, ReviewPassKind::ImportExternalContext),
            ReviewExternalObjectState::Current,
        );
        let error = link
            .confirm_unchanged(pass.clone(), pass_run_evidence(&pass))
            .expect_err("an unchanged report must name the aggregate reservation");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleObservationPass
        );
    }

    /// INV-040 / INV-041: an observation-producing pass cannot later be
    /// reinterpreted as an unchanged report.
    #[test]
    fn inv040_inv041_external_link_rejects_reused_no_change_pass() {
        let reservation = link_id(30);
        let link = attached_finding_link(finding_ref(10), reservation)
            .observe(observation_evidence(
                reservation,
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                ReviewExternalObjectState::Current,
            ))
            .expect("first external state is meaning-bearing");
        let pass = no_change_pass(
            reservation,
            succeeded_pass(21, ReviewPassKind::ImportExternalContext),
            ReviewExternalObjectState::Current,
        );
        let error = link
            .confirm_unchanged(pass.clone(), pass_run_evidence(&pass))
            .expect_err("one pass cannot bind both an observation and no-change result");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::ConflictingPassEvidence
        );
    }

    /// INV-040 / INV-041: a target-associated pending reservation can consume
    /// one blocked publication pass.
    #[test]
    fn inv040_inv041_external_link_accepts_target_publication_block() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let pending = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Target(target),
            ReviewExternalObjectKind::Review,
        );
        let pass = publication_blocked_pass(
            pending.id(),
            blocked_pass(20, ReviewPassKind::Publish),
            text("external acknowledgement was lost"),
        );
        let blocked = pending
            .block_publication(pass.clone(), pass_run_evidence(&pass))
            .expect("the exact pending reservation authenticates the publication block");

        assert_eq!(
            blocked.association(),
            ReviewExternalLinkAssociation::Target(target)
        );
        assert!(blocked.attachment().is_none());
        assert_eq!(blocked.claims().len(), 1);
        assert_eq!(blocked.claims()[0].pass_evidence(), &pass);
        let reconstituted = reconstitute_link(&blocked);
        assert_eq!(reconstituted, blocked);
        let error = reconstituted
            .attach(attachment_evidence(
                link_id(30),
                succeeded_pass(20, ReviewPassKind::Publish),
                key("external-review"),
            ))
            .expect_err("reconstitution retains the blocked pass claim");
        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::ConflictingPassEvidence
        );
    }

    /// INV-040 / INV-041: a finding-associated pending reservation accepts
    /// the blocked lifecycle event committed by its publication pass.
    #[test]
    fn inv040_inv041_external_link_accepts_finding_publication_block() {
        let finding = finding_ref(10);
        let reservation = link_id(30);
        let pending = pending_link(
            reservation,
            ReviewExternalLinkAssociation::Finding(finding),
            ReviewExternalObjectKind::ReviewComment,
        );
        let event_kind = ReviewFindingEventKind::BlockedWithReason {
            reason: text("external acknowledgement was lost"),
            link: Some(Box::new(pending_finding_link_ref(finding, reservation))),
        };
        let pass = pass_with_finding_event(
            finding,
            ReviewEventOrdinal::one(),
            blocked_pass(20, ReviewPassKind::Publish),
            &event_kind,
        );
        let blocked = pending
            .block_publication(pass.clone(), pass_run_evidence(&pass))
            .expect("the finding event authenticates its exact pending reservation");

        assert_eq!(blocked.claims().len(), 1);
        assert_eq!(blocked.claims()[0].pass_evidence(), &pass);
    }

    /// INV-040 / INV-041: a blocked publication cannot name another pending
    /// reservation.
    #[test]
    fn inv040_inv041_external_link_rejects_foreign_publication_block() {
        let pending = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Run(pass_ref(20).run()),
            ReviewExternalObjectKind::Review,
        );
        let pass = publication_blocked_pass(
            link_id(31),
            blocked_pass(20, ReviewPassKind::Publish),
            text("external acknowledgement was lost"),
        );
        let error = pending
            .block_publication(pass.clone(), pass_run_evidence(&pass))
            .expect_err("a publication block must name the aggregate reservation");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatiblePublicationBlockPass
        );
    }

    /// INV-040 / INV-041: an attachment row retains its independently stored
    /// producing-pass identity.
    #[test]
    fn inv040_inv041_external_link_rejects_substituted_attachment_pass_identity() {
        let pass = pass_with_attachment_result(
            succeeded_pass(20, ReviewPassKind::Publish),
            link_id(30),
            &key("external-comment-42"),
        );
        let substituted = ReviewPassRef::new(pass.reference().run(), pass_id(99));
        let error = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(ReviewExternalLinkAttachment::new(
            link_id(30),
            substituted,
            pass.clone(),
            pass_run_evidence(&pass),
            key("external-comment-42"),
        ))
        .expect_err("stored attachment pass identity cannot be reattributed");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::AttachmentPassEvidenceMismatch
        );
    }

    /// INV-040 / INV-041: an observation row retains its independently stored
    /// observing-pass identity.
    #[test]
    fn inv040_inv041_external_link_rejects_substituted_observation_pass_identity() {
        let observation = observation_evidence(
            link_id(30),
            ReviewEventOrdinal::one(),
            succeeded_pass(21, ReviewPassKind::ImportExternalContext),
            ReviewExternalObjectState::Current,
        );
        let substituted = ReviewPassRef::new(observation.pass().run(), pass_id(99));
        let error = attached_finding_link(finding_ref(10), link_id(30))
            .observe(ReviewExternalLinkObservation::new(
                observation.link(),
                observation.ordinal(),
                substituted,
                observation.pass_evidence().clone(),
                observation.run_evidence(),
                observation.state(),
            ))
            .expect_err("stored observation pass identity cannot be reattributed");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::ObservationPassEvidenceMismatch
        );
    }

    /// INV-041: attachment evidence must be joined to the exact canonical run
    /// that owns its pass.
    #[test]
    fn inv041_external_link_rejects_cross_wired_attachment_run() {
        let pass = pass_with_attachment_result(
            succeeded_pass(20, ReviewPassKind::Publish),
            link_id(30),
            &key("external-comment-42"),
        );
        let run = ReviewRunEvidence::new(
            pass.reference().run(),
            ReviewWorkflowKind::JudgeFindings,
            pass.policy(),
            pass_run_evidence(&pass).state(),
        );
        let error = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(ReviewExternalLinkAttachment::new(
            link_id(30),
            pass.reference(),
            pass,
            run,
            key("external-comment-42"),
        ))
        .expect_err("attachment pass workflow must come from its owning run");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleAttachmentRunEvidence
        );
    }

    /// INV-040 / INV-041: a successful attachment pass cannot be paired with a
    /// failed canonical run projection.
    #[test]
    fn inv040_inv041_external_link_rejects_attachment_run_lifecycle_mismatch() {
        let pass = pass_with_attachment_result(
            succeeded_pass(20, ReviewPassKind::Publish),
            link_id(30),
            &key("external-comment-42"),
        );
        let run = ReviewRunEvidence::new(
            pass.reference().run(),
            ReviewWorkflowKind::PublishReview,
            pass.policy(),
            ReviewRunState::Failed {
                failed_pass: pass.reference(),
            },
        );
        let error = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(ReviewExternalLinkAttachment::new(
            link_id(30),
            pass.reference(),
            pass,
            run,
            key("external-comment-42"),
        ))
        .expect_err("a succeeded pass contradicts a failed canonical run");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleAttachmentRunEvidence
        );
    }

    /// INV-041: observation evidence must be joined to the exact canonical run
    /// that owns its pass.
    #[test]
    fn inv041_external_link_rejects_cross_wired_observation_run() {
        let pass = succeeded_pass(21, ReviewPassKind::ImportExternalContext);
        let run = ReviewRunEvidence::new(
            pass.reference().run(),
            ReviewWorkflowKind::PublishReview,
            pass.policy(),
            pass_run_evidence(&pass).state(),
        );
        let error = attached_finding_link(finding_ref(10), link_id(30))
            .observe(ReviewExternalLinkObservation::new(
                link_id(30),
                ReviewEventOrdinal::one(),
                pass.reference(),
                pass,
                run,
                ReviewExternalObjectState::Current,
            ))
            .expect_err("observation pass workflow must come from its owning run");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleObservationRunEvidence
        );
    }

    /// INV-040 / INV-041: a successful observation pass cannot be paired with
    /// a still-running canonical run projection.
    #[test]
    fn inv040_inv041_external_link_rejects_observation_run_lifecycle_mismatch() {
        let pass = observation_evidence(
            link_id(30),
            ReviewEventOrdinal::one(),
            succeeded_pass(21, ReviewPassKind::ImportExternalContext),
            ReviewExternalObjectState::Current,
        )
        .pass_evidence
        .clone();
        let run = ReviewRunEvidence::new(
            pass.reference().run(),
            ReviewWorkflowKind::ImportExternalContext,
            pass.policy(),
            ReviewRunState::Running {
                active_pass: pass.reference(),
            },
        );
        let error = attached_finding_link(finding_ref(10), link_id(30))
            .observe(ReviewExternalLinkObservation::new(
                link_id(30),
                ReviewEventOrdinal::one(),
                pass.reference(),
                pass,
                run,
                ReviewExternalObjectState::Current,
            ))
            .expect_err("a succeeded pass contradicts a running canonical run");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleObservationRunEvidence
        );
    }

    /// INV-040: a pass identity reused across observations cannot change its
    /// terminal evidence.
    #[test]
    fn inv040_external_link_rejects_conflicting_reused_observation_pass() {
        let first = succeeded_pass(21, ReviewPassKind::ImportExternalContext);
        let link = attached_finding_link(finding_ref(10), link_id(30))
            .observe(observation_evidence(
                link_id(30),
                ReviewEventOrdinal::one(),
                first.clone(),
                ReviewExternalObjectState::Current,
            ))
            .expect("first canonical observation is admitted");
        let changed = ReviewPassEvidence::new(
            first.reference(),
            first.kind(),
            first.policy(),
            ReviewPassState::Succeeded {
                turn: turn_id(121),
                output_frontier: frontier_id(999),
                result: None,
            },
        );
        let error = link
            .observe(observation_evidence(
                link_id(30),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                changed,
                ReviewExternalObjectState::Outdated,
            ))
            .expect_err("one pass identity cannot change terminal evidence");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::ConflictingPassEvidence
        );
    }

    /// INV-040: a user-global external-effect pass cannot move to another
    /// run.
    #[test]
    fn inv040_external_link_rejects_reparented_pass_identity() {
        let attachment_pass = succeeded_pass(ARBITRARY_JUDGMENT_PASS_SEED, ReviewPassKind::Publish);
        let attached = attached_finding_link_with_pass(
            finding_ref(CANONICAL_FINDING_SEED),
            link_id(ARBITRARY_LINK_SEED),
            attachment_pass.clone(),
        );
        let observing_pass = ReviewPassEvidence::new(
            ReviewPassRef::new(
                pass_ref(REASSIGNED_PASS_SEED).run(),
                attachment_pass.reference().pass(),
            ),
            ReviewPassKind::ImportExternalContext,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(CROSS_WIRED_TURN_SEED),
                output_frontier: frontier_id(CROSS_WIRED_TURN_SEED),
                result: None,
            },
        );
        let error = attached
            .observe(observation_evidence(
                link_id(ARBITRARY_LINK_SEED),
                ReviewEventOrdinal::one(),
                observing_pass,
                ReviewExternalObjectState::Current,
            ))
            .expect_err("one pass identity cannot move between runs");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::ConflictingPassEvidence
        );
    }

    /// INV-040: an external-effect run cannot change its one pass or workflow.
    #[test]
    fn inv040_external_link_rejects_changed_run_claim() {
        let attachment_pass = succeeded_pass(ARBITRARY_JUDGMENT_PASS_SEED, ReviewPassKind::Publish);
        let attached = attached_finding_link_with_pass(
            finding_ref(CANONICAL_FINDING_SEED),
            link_id(ARBITRARY_LINK_SEED),
            attachment_pass.clone(),
        );
        let observing_pass = ReviewPassEvidence::new(
            ReviewPassRef::new(
                attachment_pass.reference().run(),
                pass_id(REASSIGNED_PASS_SEED),
            ),
            ReviewPassKind::ImportExternalContext,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(CROSS_WIRED_TURN_SEED),
                output_frontier: frontier_id(CROSS_WIRED_TURN_SEED),
                result: None,
            },
        );
        let error = attached
            .observe(observation_evidence(
                link_id(ARBITRARY_LINK_SEED),
                ReviewEventOrdinal::one(),
                observing_pass,
                ReviewExternalObjectState::Current,
            ))
            .expect_err("one run identity cannot change pass or workflow");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::ConflictingRunEvidence
        );
    }

    /// INV-040: an observation from another same-target link cannot be
    /// attributed to the loaded aggregate.
    #[test]
    fn inv040_external_link_rejects_foreign_observation_owner() {
        let attached = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(attachment_evidence(
            link_id(30),
            succeeded_pass(20, ReviewPassKind::Publish),
            key("external-comment-42"),
        ))
        .expect("same-target pass may attach the reservation");
        let error = attached
            .observe(ReviewExternalLinkObservation::new(
                link_id(31),
                ReviewEventOrdinal::one(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext).reference(),
                succeeded_pass(21, ReviewPassKind::ImportExternalContext),
                pass_run_evidence(&succeeded_pass(21, ReviewPassKind::ImportExternalContext)),
                ReviewExternalObjectState::Current,
            ))
            .expect_err("observation owner must match the aggregate link");
        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::ForeignObservationLink
        );
    }

    /// INV-041: an attachment from another same-target reservation cannot be
    /// attributed to the loaded aggregate.
    #[test]
    fn inv041_external_link_rejects_foreign_attachment_owner() {
        let pending = pending_link(
            link_id(30),
            ReviewExternalLinkAssociation::Finding(finding_ref(10)),
            ReviewExternalObjectKind::ReviewComment,
        );
        let error = pending
            .attach(ReviewExternalLinkAttachment::new(
                link_id(31),
                succeeded_pass(20, ReviewPassKind::Publish).reference(),
                succeeded_pass(20, ReviewPassKind::Publish),
                pass_run_evidence(&succeeded_pass(20, ReviewPassKind::Publish)),
                key("external-comment-42"),
            ))
            .expect_err("attachment owner must match the aggregate link");
        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::ForeignAttachmentLink
        );
    }

    /// INV-040: produced-finding inventories have one canonical identity
    /// order.
    #[test]
    fn inv040_produced_findings_canonicalize_identity_order() {
        let first = finding_ref(10);
        let second = finding_ref(11);
        let inventory = ReviewProducedFindings::try_new(vec![second, first])
            .expect("distinct finding identities are canonicalizable");

        assert_eq!(inventory.findings(), &[first, second]);
    }

    /// INV-040: a produced-finding inventory cannot repeat an identity.
    #[test]
    fn inv040_produced_findings_reject_duplicate_identity() {
        let finding = finding_ref(10);
        let error = ReviewProducedFindings::try_new(vec![finding, finding])
            .expect_err("one result cannot repeat a finding identity");

        assert_eq!(error, ReviewProducedFindingsError::Duplicate { finding });
    }

    /// INV-040: a produced-finding inventory is bounded to 32 identities.
    #[test]
    fn inv040_produced_findings_reject_over_budget_inventory() {
        let inventory = over_budget_finding_inventory();
        let actual = inventory.len();
        let error = ReviewProducedFindings::try_new(inventory)
            .expect_err("the defensive result budget is exact");

        assert_eq!(
            error,
            ReviewProducedFindingsError::TooMany {
                actual,
                maximum: REVIEW_PRODUCED_FINDINGS_MAXIMUM,
            }
        );
    }

    /// INV-040: a finding must appear in its exact producing pass inventory.
    #[test]
    fn inv040_finding_proposal_rejects_omitted_producing_result() {
        let empty = ReviewPassEvidence::new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(103),
                output_frontier: frontier_id(203),
                result: Some(ReviewPassResult::ProducedFindings(
                    ReviewProducedFindings::try_new(Vec::new())
                        .expect("empty inventory is canonical"),
                )),
            },
        );
        let error = ReviewFindingProposal::try_new(
            finding_ref(10),
            empty,
            run_evidence(),
            &target_with_base(),
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect_err("a pass cannot produce a finding omitted from its result");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleProducingPassEvidence
        );
    }

    /// INV-040: a proposal rejects a result inventory containing a finding
    /// attributed to another producing pass.
    #[test]
    fn inv040_finding_proposal_rejects_cross_wired_producing_inventory() {
        let reference = finding_ref(10);
        let foreign = ReviewFindingRef::new(pass_ref(4), finding_id(11));
        let producing_pass = ReviewPassEvidence::new(
            reference.pass(),
            ReviewPassKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(103),
                output_frontier: frontier_id(203),
                result: Some(ReviewPassResult::ProducedFindings(
                    ReviewProducedFindings::try_new(vec![reference, foreign])
                        .expect("distinct finding identities are canonical"),
                )),
            },
        );
        let error = ReviewFindingProposal::try_new(
            reference,
            producing_pass,
            run_evidence(),
            &target_with_base(),
            finding_content(Some(ReviewFindingDiffSide::Right)),
        )
        .expect_err("every inventory member must belong to the producing pass");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleProducingPassEvidence
        );
    }

    /// INV-040: a durably reconstituted completed review always carries its
    /// exact produced-finding inventory, including the empty inventory.
    #[test]
    fn inv040_read_only_pass_reconstitution_requires_produced_findings() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            ),
            ReviewPassReconstitutionFailure::IncompatibleResult,
        );
    }

    /// INV-040: a pass result cannot name a finding from another target.
    #[test]
    fn inv040_pass_reconstitution_rejects_foreign_result_target() {
        let foreign_finding = ReviewFindingRef::new(
            ReviewPassRef::new(ReviewRunRef::new(target_id(99), run_id(2)), pass_id(3)),
            finding_id(10),
        );
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(
                    accepted_input_id(5),
                    session_id(4),
                    Some(turn_id(6)),
                ),
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: Some(ReviewPassResult::ProducedFindings(
                        ReviewProducedFindings::try_new(vec![foreign_finding])
                            .expect("foreign identity still has canonical ordering"),
                    )),
                },
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            ),
            ReviewPassReconstitutionFailure::ForeignResultTarget,
        );
    }

    /// INV-040: a terminal pass may bind its exact result once.
    #[test]
    fn inv040_terminal_pass_binds_absent_result() {
        let result = accepted_finding_result();
        let bound = succeeded_judgment_pass()
            .bind_result(result.clone())
            .expect("compatible exact result binds");

        assert_eq!(bound.state().result(), Some(&result));
    }

    /// INV-040: replaying the equal bound pass result is idempotent.
    #[test]
    fn inv040_terminal_pass_accepts_equal_result_replay() {
        let result = accepted_finding_result();
        let bound = succeeded_judgment_pass()
            .bind_result(result.clone())
            .expect("compatible exact result binds");

        assert_eq!(
            bound
                .clone()
                .bind_result(result)
                .expect("equal replay observes the existing result"),
            bound
        );
    }

    /// INV-040: a distinct result cannot replace an already bound result.
    #[test]
    fn inv040_terminal_pass_rejects_distinct_result_rebind() {
        let bound = succeeded_judgment_pass()
            .bind_result(accepted_finding_result())
            .expect("compatible exact result binds");
        let replacement = ReviewPassResult::FindingEvent(ReviewFindingEventResult::new(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            ReviewFindingEventResultKind::Rejected {
                reason: text("another disposition"),
            },
        ));
        let error = bound
            .bind_result(replacement)
            .expect_err("a bound result is immutable");

        assert_eq!(
            error.failure(),
            ReviewPassTransitionFailure::ResultAlreadyBound
        );
    }

    /// INV-040: a rejected run transition returns its unchanged aggregate.
    #[test]
    fn inv040_rejected_run_transition_retains_current_aggregate() {
        let current = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let error = current
            .clone()
            .transition(ReviewRunState::Queued, None)
            .expect_err("a run cannot transition from queued back to queued");

        assert_eq!(error.current(), &current);
    }

    /// INV-040: a rejected pass transition returns its unchanged aggregate.
    #[test]
    fn inv040_rejected_pass_transition_retains_current_aggregate() {
        let current = succeeded_review_pass();
        let error = current
            .clone()
            .transition(current.state().clone(), None)
            .expect_err("a terminal pass cannot transition again");

        assert_eq!(error.current(), &current);
    }

    /// INV-040: a rejected finding event returns its unchanged aggregate.
    #[test]
    fn inv040_rejected_finding_transition_retains_current_aggregate() {
        let current = ReviewFinding::new(proposal());
        let foreign = finding_event(
            finding_ref(FOREIGN_FINDING_SEED),
            ReviewEventOrdinal::one(),
            succeeded_pass(ARBITRARY_JUDGMENT_PASS_SEED, ReviewPassKind::Judge),
            ReviewFindingEventKind::Accepted,
        );
        let error = current
            .clone()
            .apply(foreign)
            .expect_err("another finding's event cannot mutate this aggregate");

        assert_eq!(error.current(), Some(&current));
    }

    /// INV-041: a rejected link transition returns its unchanged aggregate.
    #[test]
    fn inv041_rejected_link_transition_retains_current_aggregate() {
        let link = link_id(ARBITRARY_LINK_SEED);
        let current = pending_link(
            link,
            ReviewExternalLinkAssociation::Finding(finding_ref(CANONICAL_FINDING_SEED)),
            ReviewExternalObjectKind::ReviewComment,
        );
        let observation = observation_evidence(
            link,
            ReviewEventOrdinal::one(),
            succeeded_pass(
                ARBITRARY_JUDGMENT_PASS_SEED,
                ReviewPassKind::ImportExternalContext,
            ),
            ReviewExternalObjectState::Current,
        );
        let error = current
            .clone()
            .observe(observation)
            .expect_err("an unattached link rejects observation");

        assert_eq!(error.current(), &current);
    }

    /// INV-040: read-only success admission includes its exact finding inventory.
    #[test]
    fn inv040_read_only_success_transition_requires_produced_findings() {
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let running = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
        )
        .expect("canonical input admits the pass")
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
        .expect("active canonical turn starts the pass");
        let error = running
            .transition(
                ReviewPassState::Succeeded {
                    turn: turn_id(6),
                    output_frontier: frontier_id(8),
                    result: None,
                },
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::Completed,
                    Some(frontier_id(8)),
                )),
            )
            .expect_err("read-only success must atomically bind its inventory");

        assert_eq!(
            error.failure(),
            ReviewPassTransitionFailure::IncompatibleResult
        );
    }

    /// INV-041: a blocked publication transition must consume one exact
    /// reservation-bearing result.
    #[test]
    fn inv041_publish_block_transition_requires_result() {
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::PublishReview,
            ReviewPolicy::version_one(),
        );
        let running = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::Publish,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
        )
        .expect("canonical input admits the publish pass")
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
        .expect("active canonical turn starts the publish pass");
        let error = running
            .transition(
                ReviewPassState::Blocked {
                    turn: turn_id(6),
                    result: None,
                },
                Some(ReviewPassTurnEvidence::new(
                    turn_id(6),
                    session_id(4),
                    accepted_input_id(5),
                    ReviewPassTurnOutcome::ReconciliationRequired,
                    Some(frontier_id(8)),
                )),
            )
            .expect_err("blocked publication must bind its exact reservation");
        assert_eq!(
            error.failure(),
            ReviewPassTransitionFailure::IncompatibleResult
        );
    }

    /// INV-041: reconstitution also fails closed when a blocked publication
    /// row omits its reservation-bearing result.
    #[test]
    fn inv041_publish_block_reconstitution_requires_result() {
        let state = ReviewPassState::Blocked {
            turn: turn_id(6),
            result: None,
        };
        let input = ReviewPassReconstitutionInput::new(
            pass_ref(3),
            ReviewPassKind::Publish,
            run_ref(),
            ReviewWorkflowKind::PublishReview,
            session_id(4),
            accepted_input_id(5),
            ReviewPassAcceptedInputEvidence::new(
                accepted_input_id(5),
                session_id(4),
                Some(turn_id(6)),
            ),
            state,
            Some(ReviewPassTurnEvidence::new(
                turn_id(6),
                session_id(4),
                accepted_input_id(5),
                ReviewPassTurnOutcome::ReconciliationRequired,
                Some(frontier_id(8)),
            )),
        );
        assert_pass_reconstitution_rejects(
            input,
            ReviewPassReconstitutionFailure::IncompatibleResult,
        );
    }

    /// INV-040: pass construction requires an accepted input with a canonical
    /// origin turn.
    #[test]
    fn inv040_pass_construction_rejects_input_without_origin_turn() {
        let mut run = ReviewRun::new(
            run_ref(),
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        );
        let error = ReviewPass::try_new(
            pass_ref(3),
            ReviewPassKind::ReadOnlyReview,
            &mut run,
            session_id(4),
            ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), None),
        )
        .expect_err("pending or consumed steering cannot back a pass");

        assert_eq!(
            error.failure(),
            ReviewPassConstructionFailure::AcceptedInputHasNoOriginTurn
        );
    }

    /// INV-040: pass reconstitution rejects accepted input that no longer
    /// authenticates an origin turn.
    #[test]
    fn inv040_pass_reconstitution_rejects_input_without_origin_turn() {
        assert_pass_reconstitution_rejects(
            ReviewPassReconstitutionInput::new(
                pass_ref(3),
                ReviewPassKind::ReadOnlyReview,
                run_ref(),
                ReviewWorkflowKind::ReadOnlyReview,
                session_id(4),
                accepted_input_id(5),
                ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), None),
                ReviewPassState::Queued,
                None,
            ),
            ReviewPassReconstitutionFailure::AcceptedInputHasNoOriginTurn,
        );
    }

    /// INV-040: rejection reason is part of the exact event result.
    #[test]
    fn inv040_finding_history_rejects_mismatched_result_reason() {
        let finding = finding_ref(10);
        let ordinal = ReviewEventOrdinal::one();
        let committed = ReviewFindingEventKind::Rejected {
            reason: text("committed reason"),
        };
        let pass = pass_with_finding_event(
            finding,
            ordinal,
            succeeded_pass(20, ReviewPassKind::Judge),
            &committed,
        );
        let event = ReviewFindingEvent::new(
            finding,
            ordinal,
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            ReviewFindingEventKind::Rejected {
                reason: text("different reason"),
            },
        );
        let error = ReviewFinding::new(proposal())
            .apply(event)
            .expect_err("event reason must equal the pass result");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleEventPassEvidence
        );
    }

    /// INV-040: a duplicate's referenced identity is part of the exact event
    /// result.
    #[test]
    fn inv040_finding_history_rejects_mismatched_result_reference() {
        let finding = finding_ref(10);
        let ordinal = ReviewEventOrdinal::one();
        let committed = ReviewFindingEventKind::Duplicate {
            canonical: referenced_finding(11, ReviewFindingStatus::Open),
        };
        let pass = pass_with_finding_event(
            finding,
            ordinal,
            succeeded_pass(20, ReviewPassKind::Dedupe),
            &committed,
        );
        let event = ReviewFindingEvent::new(
            finding,
            ordinal,
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            ReviewFindingEventKind::Duplicate {
                canonical: referenced_finding(12, ReviewFindingStatus::Open),
            },
        );
        let error = ReviewFinding::new(proposal())
            .apply(event)
            .expect_err("referenced identity must equal the pass result");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleEventPassEvidence
        );
    }

    /// INV-040: a duplicate's authenticated admission status is part of the
    /// exact event result.
    #[test]
    fn inv040_finding_history_rejects_mismatched_result_reference_status() {
        let finding = finding_ref(10);
        let ordinal = ReviewEventOrdinal::one();
        let committed = ReviewFindingEventKind::Duplicate {
            canonical: referenced_finding(11, ReviewFindingStatus::Open),
        };
        let pass = pass_with_finding_event(
            finding,
            ordinal,
            succeeded_pass(20, ReviewPassKind::Dedupe),
            &committed,
        );
        let event = ReviewFindingEvent::new(
            finding,
            ordinal,
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            ReviewFindingEventKind::Duplicate {
                canonical: referenced_finding(11, ReviewFindingStatus::Accepted),
            },
        );
        let error = ReviewFinding::new(proposal())
            .apply(event)
            .expect_err("authenticated status must equal the pass result");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::IncompatibleEventPassEvidence
        );
    }

    /// INV-040: judgment cannot accept a finding below the frozen threshold.
    #[test]
    fn inv040_finding_rejects_acceptance_below_policy_threshold() {
        let error = ReviewFinding::new(proposal_with_confidence_axes(FindingConfidenceAxes {
            is_real: 6_999,
            severity_label: 10_000,
        }))
        .apply(finding_event(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            succeeded_pass(20, ReviewPassKind::Judge),
            ReviewFindingEventKind::Accepted,
        ))
        .expect_err("is-real confidence below 70 percent cannot be accepted");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::BelowJudgmentThreshold
        );
    }

    /// INV-040: publication cannot post a finding below the frozen threshold.
    #[test]
    fn inv040_finding_rejects_posting_below_policy_threshold() {
        let finding = ReviewFinding::new(proposal_with_confidence_axes(FindingConfidenceAxes {
            is_real: 7_999,
            severity_label: 10_000,
        }))
        .apply(finding_event(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            succeeded_pass(19, ReviewPassKind::Judge),
            ReviewFindingEventKind::Accepted,
        ))
        .expect("is-real confidence meets the judgment threshold");
        let error = finding
            .apply(posted_finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                succeeded_pass(20, ReviewPassKind::Publish),
                link_id(30),
            ))
            .expect_err("is-real confidence below 80 percent cannot be posted");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::BelowPublicationThreshold
        );
    }

    /// INV-040: severity-label uncertainty never suppresses a real finding.
    #[test]
    fn inv040_finding_thresholds_ignore_severity_label_confidence() {
        let accepted = ReviewFinding::new(proposal_with_confidence_axes(FindingConfidenceAxes {
            is_real: 9_500,
            severity_label: 0,
        }))
        .apply(finding_event(
            finding_ref(10),
            ReviewEventOrdinal::one(),
            succeeded_pass(19, ReviewPassKind::Judge),
            ReviewFindingEventKind::Accepted,
        ))
        .expect("high is-real confidence permits judgment");

        let posted = accepted
            .apply(posted_finding_event(
                finding_ref(10),
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
                succeeded_pass(20, ReviewPassKind::Publish),
                link_id(30),
            ))
            .expect("severity-label uncertainty does not suppress publication");

        assert_eq!(posted.status(), ReviewFindingStatus::Posted);
    }

    /// INV-041: a reservation provider must be the canonical target provider.
    #[test]
    fn inv041_external_link_rejects_provider_mismatch() {
        let error = ReviewExternalLink::try_reserve(
            link_id(30),
            ReviewExternalLinkAssociation::Target(target_id(1)),
            key("other-host"),
            ReviewExternalObjectKind::Review,
            &target_with_base(),
        )
        .expect_err("caller-supplied provider cannot contradict the target");

        assert_eq!(error, ReviewExternalLinkTransitionFailure::ProviderMismatch);
    }

    /// INV-041: a posted attachment result must belong to the exact finding
    /// association it commits.
    #[test]
    fn inv041_external_link_rejects_posted_result_for_target_association() {
        let finding = finding_ref(10);
        let link = link_id(30);
        let external_object = key("external-comment-42");
        let pass = pass_with_posted_attachment_result(
            finding,
            ReviewEventOrdinal::one(),
            succeeded_pass(20, ReviewPassKind::Publish),
            link,
            &external_object,
        );
        let error = pending_link(
            link,
            ReviewExternalLinkAssociation::Target(target_id(1)),
            ReviewExternalObjectKind::ReviewComment,
        )
        .attach(ReviewExternalLinkAttachment::new(
            link,
            pass.reference(),
            pass.clone(),
            pass_run_evidence(&pass),
            external_object,
        ))
        .expect_err("posted result cannot attach through a target-only link");

        assert_eq!(
            error.failure(),
            ReviewExternalLinkTransitionFailure::IncompatibleAttachmentPass
        );
    }

    /// INV-041: the same canonical object may follow one moving change request
    /// to a refreshed target snapshot.
    #[test]
    fn inv041_external_object_claim_accepts_refreshed_change_request() {
        let first_target = change_request_target(ChangeRequestTargetFixture {
            target_seed: 1,
            provider: "code-host",
            repository: "repository",
            number: 42,
            head: "head-one",
        });
        let next_target = change_request_target(ChangeRequestTargetFixture {
            target_seed: 2,
            provider: "code-host",
            repository: "repository",
            number: 42,
            head: "head-two",
        });
        let first = ReviewExternalObjectClaim::try_new(
            &attached_target_link(
                &first_target,
                AttachedTargetLinkFixture {
                    link_seed: 30,
                    run_seed: 40,
                    pass_seed: 50,
                    external_object: "review-42",
                },
            ),
            &first_target,
        )
        .expect("first attachment establishes the logical claim");
        let next = ReviewExternalObjectClaim::try_new(
            &attached_target_link(
                &next_target,
                AttachedTargetLinkFixture {
                    link_seed: 31,
                    run_seed: 41,
                    pass_seed: 51,
                    external_object: "review-42",
                },
            ),
            &next_target,
        )
        .expect("refreshed target has its own attached claim");

        first
            .validate_reassociation(&next)
            .expect("same change request may retain the provider object");
    }

    /// INV-041: independently loaded target evidence cannot contradict the
    /// provider frozen by the external-link reservation.
    #[test]
    fn inv041_external_object_claim_rejects_cross_wired_target_provider() {
        let target = target_with_base();
        let link = attached_target_link(
            &target,
            AttachedTargetLinkFixture {
                link_seed: 30,
                run_seed: 40,
                pass_seed: 50,
                external_object: "review-42",
            },
        );
        let cross_wired = ReviewTarget::try_new(
            target.id(),
            key("other-code-host"),
            target.repository().clone(),
            target.subject(),
            target.head_revision().clone(),
            target.base_revision().cloned(),
            None,
        )
        .expect("the independently loaded target shape is otherwise canonical");

        assert_eq!(
            ReviewExternalObjectClaim::try_new(&link, &cross_wired),
            Err(ReviewExternalObjectClaimError::ProviderMismatch)
        );
    }

    /// INV-041: one canonical object cannot move to an unrelated change
    /// request.
    #[test]
    fn inv041_external_object_claim_rejects_unrelated_change_request() {
        let first_target = change_request_target(ChangeRequestTargetFixture {
            target_seed: 1,
            provider: "code-host",
            repository: "repository",
            number: 42,
            head: "head-one",
        });
        let other_target = change_request_target(ChangeRequestTargetFixture {
            target_seed: 2,
            provider: "code-host",
            repository: "repository",
            number: 43,
            head: "head-two",
        });
        let first = ReviewExternalObjectClaim::try_new(
            &attached_target_link(
                &first_target,
                AttachedTargetLinkFixture {
                    link_seed: 30,
                    run_seed: 40,
                    pass_seed: 50,
                    external_object: "review-42",
                },
            ),
            &first_target,
        )
        .expect("first attachment establishes the logical claim");
        let other = ReviewExternalObjectClaim::try_new(
            &attached_target_link(
                &other_target,
                AttachedTargetLinkFixture {
                    link_seed: 31,
                    run_seed: 41,
                    pass_seed: 51,
                    external_object: "review-42",
                },
            ),
            &other_target,
        )
        .expect("other target has its own attached claim");

        assert_eq!(
            first.validate_reassociation(&other),
            Err(ReviewExternalObjectClaimError::UnrelatedTarget)
        );
    }

    /// INV-041: immutable commit snapshots never share one canonical external
    /// object.
    #[test]
    fn inv041_external_object_claim_rejects_commit_reassociation() {
        let first_target = ReviewTarget::try_new(
            target_id(1),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("head-one"),
            None,
            None,
        )
        .expect("first commit target is canonical");
        let next_target = ReviewTarget::try_new(
            target_id(2),
            key("code-host"),
            key("repository"),
            ReviewTargetSubject::Commit,
            key("head-two"),
            None,
            None,
        )
        .expect("next commit target is canonical");
        let first = ReviewExternalObjectClaim::try_new(
            &attached_target_link(
                &first_target,
                AttachedTargetLinkFixture {
                    link_seed: 30,
                    run_seed: 40,
                    pass_seed: 50,
                    external_object: "review-42",
                },
            ),
            &first_target,
        )
        .expect("first attachment establishes the logical claim");
        let next = ReviewExternalObjectClaim::try_new(
            &attached_target_link(
                &next_target,
                AttachedTargetLinkFixture {
                    link_seed: 31,
                    run_seed: 41,
                    pass_seed: 51,
                    external_object: "review-42",
                },
            ),
            &next_target,
        )
        .expect("next target has its own attached claim");

        assert_eq!(
            first.validate_reassociation(&next),
            Err(ReviewExternalObjectClaimError::UnrelatedTarget)
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
    fn unknown_policy_version_is_rejected() {
        let error = ReviewPolicy::try_new(
            ReviewPolicyVersion::try_new(2).expect("positive version"),
            ReviewConfidence::try_from_basis_points(7_000).expect("bounded confidence"),
            ReviewConfidence::try_from_basis_points(8_000).expect("bounded confidence"),
        )
        .expect_err("unknown policy versions fail closed");
        assert_eq!(
            error.into_parts(),
            (
                ReviewPolicyVersion::try_new(2).expect("positive version"),
                ReviewConfidence::try_from_basis_points(7_000).expect("bounded confidence"),
                ReviewConfidence::try_from_basis_points(8_000).expect("bounded confidence"),
            )
        );
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

    /// INV-040: duplicate admission preserves each finding's independent
    /// producing run and pass when target and frozen policy are equal.
    #[test]
    fn inv040_finding_admits_authenticated_cross_run_reference() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let subject_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let canonical_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
        let subject = open_finding_with_producer(subject_reference, ReviewPolicy::version_one());
        let canonical =
            open_finding_with_producer(canonical_reference, ReviewPolicy::version_one());
        let canonical_evidence = ReviewReferencedFindingEvidence::try_from_finding(&canonical)
            .expect("open canonical finding is eligible reference evidence");
        let event = finding_event(
            subject_reference,
            ReviewEventOrdinal::one(),
            succeeded_pass(ARBITRARY_DEDUPE_PASS_SEED, ReviewPassKind::Dedupe),
            ReviewFindingEventKind::Duplicate {
                canonical: canonical_evidence,
            },
        );

        let duplicate = subject
            .apply(event)
            .expect("equal-target equal-policy cross-run reference is admitted");

        assert_eq!(duplicate.status(), ReviewFindingStatus::Duplicate);
        assert_eq!(
            canonical_evidence.reference(),
            canonical.proposal().reference(),
            "referenced finding retains its independent ancestry",
        );
        assert_ne!(
            duplicate.proposal().reference().run(),
            canonical.proposal().reference().run(),
            "cross-run admission must not reparent either finding",
        );
    }

    /// INV-040: a cross-run reference cannot cross immutable target identity.
    #[test]
    fn inv040_finding_rejects_cross_target_reference() {
        let subject_reference =
            finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
        let foreign_reference = finding_ref_for_producer(target_id(2), REASSIGNED_PASS_SEED);
        let subject = open_finding_with_producer(subject_reference, ReviewPolicy::version_one());
        let foreign = open_finding_with_producer(foreign_reference, ReviewPolicy::version_one());
        let foreign_evidence = ReviewReferencedFindingEvidence::try_from_finding(&foreign)
            .expect("foreign open finding still has authenticated producer evidence");
        let event = finding_event(
            subject_reference,
            ReviewEventOrdinal::one(),
            succeeded_pass(ARBITRARY_DEDUPE_PASS_SEED, ReviewPassKind::Dedupe),
            ReviewFindingEventKind::Duplicate {
                canonical: foreign_evidence,
            },
        );

        let error = subject
            .apply(event)
            .expect_err("a reference cannot cross immutable target identity");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::ForeignReferencedFinding,
        );
    }

    /// INV-040: a cross-run reference cannot cross complete frozen policy.
    #[test]
    fn inv040_finding_rejects_cross_policy_reference() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let subject_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let foreign_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
        let subject = open_finding_with_producer(subject_reference, ReviewPolicy::version_one());
        let foreign = open_finding_with_producer(foreign_reference, unsupported_policy());
        let foreign_evidence = ReviewReferencedFindingEvidence::try_from_finding(&foreign)
            .expect("open finding retains its independently authenticated later policy");
        let event = finding_event(
            subject_reference,
            ReviewEventOrdinal::one(),
            succeeded_pass(ARBITRARY_DEDUPE_PASS_SEED, ReviewPassKind::Dedupe),
            ReviewFindingEventKind::Duplicate {
                canonical: foreign_evidence,
            },
        );

        let error = subject
            .apply(event)
            .expect_err("a reference cannot cross complete frozen policy");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::ReferencedFindingPolicyMismatch,
        );
    }

    /// INV-040: complete-graph reconstitution rejects an edge whose frozen
    /// policy contradicts the independently supplied referenced root.
    #[test]
    fn inv040_reconstituted_reference_graph_rejects_root_policy_mismatch() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let subject_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let canonical_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
        let subject = open_finding_with_producer(subject_reference, ReviewPolicy::version_one());
        let canonical = open_finding_with_producer(canonical_reference, unsupported_policy());
        let forged_edge = ReviewReferencedFindingEvidence {
            reference: canonical_reference,
            status: ReviewFindingStatus::Open,
            producer_policy: ReviewPolicy::version_one(),
        };
        let subject = ReviewFinding::try_reconstitute(
            subject.proposal().clone(),
            vec![finding_event(
                subject_reference,
                ReviewEventOrdinal::one(),
                succeeded_pass(ARBITRARY_DEDUPE_PASS_SEED, ReviewPassKind::Dedupe),
                ReviewFindingEventKind::Duplicate {
                    canonical: forged_edge,
                },
            )],
        )
        .expect("forged edge is locally consistent with the subject policy");

        let error = validate_complete_review_finding_reference_graph(&[subject, canonical])
            .expect_err("edge policy must authenticate against the supplied root producer");

        assert_eq!(
            *error,
            ReviewFindingReferenceGraphError::ReferencedFindingPolicyMismatch {
                finding: subject_reference,
                referenced: canonical_reference,
            },
        );
    }

    /// INV-040: pass-result projection independently rejects cross-policy
    /// referenced evidence before finding-event admission.
    #[test]
    fn inv040_pass_result_projection_rejects_cross_policy_reference() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let subject_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let foreign_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
        let foreign = open_finding_with_producer(foreign_reference, unsupported_policy());
        let foreign_evidence = ReviewReferencedFindingEvidence::try_from_finding(&foreign)
            .expect("open finding retains its independently authenticated later policy");
        let result = ReviewPassResult::FindingEvent(ReviewFindingEventResult::new(
            subject_reference,
            ReviewEventOrdinal::one(),
            ReviewFindingEventResultKind::Duplicate {
                canonical: foreign_evidence,
            },
        ));
        let event_pass = succeeded_pass(ARBITRARY_DEDUPE_PASS_SEED, ReviewPassKind::Dedupe);

        assert!(
            event_pass.project_result(result).is_none(),
            "event-pass policy must equal referenced-producer policy",
        );
    }

    /// INV-040: referenced-finding reconstitution authenticates every element
    /// of target/run/pass/finding ancestry against the sealed producer.
    #[test]
    fn inv040_referenced_finding_reconstitution_rejects_cross_wired_ancestry() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let canonical_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let canonical =
            open_finding_with_producer(canonical_reference, ReviewPolicy::version_one());
        let producing_pass = canonical.proposal().producing_pass();
        let producing_run = pass_run_evidence(producing_pass);
        let foreign_target_reference = ReviewFindingRef::new(
            ReviewPassRef::new(
                ReviewRunRef::new(target_id(2), canonical_reference.run().run()),
                canonical_reference.pass().pass(),
            ),
            canonical_reference.finding(),
        );
        let foreign_run_reference = ReviewFindingRef::new(
            ReviewPassRef::new(
                ReviewRunRef::new(target, run_id(99)),
                canonical_reference.pass().pass(),
            ),
            canonical_reference.finding(),
        );
        let foreign_pass_reference = ReviewFindingRef::new(
            ReviewPassRef::new(canonical_reference.run(), pass_id(99)),
            canonical_reference.finding(),
        );
        let foreign_finding_reference =
            ReviewFindingRef::new(canonical_reference.pass(), finding_id(99));

        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                foreign_target_reference,
                ReviewFindingStatus::Open,
                producing_pass,
                producing_run,
            )
            .is_none(),
            "cross-wired target ancestry must fail closed",
        );
        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                foreign_run_reference,
                ReviewFindingStatus::Open,
                producing_pass,
                producing_run,
            )
            .is_none(),
            "cross-wired run ancestry must fail closed",
        );
        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                foreign_pass_reference,
                ReviewFindingStatus::Open,
                producing_pass,
                producing_run,
            )
            .is_none(),
            "cross-wired pass ancestry must fail closed",
        );
        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                foreign_finding_reference,
                ReviewFindingStatus::Open,
                producing_pass,
                producing_run,
            )
            .is_none(),
            "finding absent from the sealed inventory must fail closed",
        );
    }

    /// INV-040: referenced-finding reconstitution rejects a producer inventory
    /// containing an identity owned by another pass.
    #[test]
    fn inv040_referenced_finding_reconstitution_rejects_unsealed_inventory() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let foreign_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
        let producing_pass = ReviewPassEvidence::new(
            reference.pass(),
            ReviewPassKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(6),
                output_frontier: frontier_id(8),
                result: Some(ReviewPassResult::ProducedFindings(
                    ReviewProducedFindings::try_new(vec![reference, foreign_reference])
                        .expect("distinct references have canonical identity order"),
                )),
            },
        );
        let producing_run = pass_run_evidence(&producing_pass);

        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                reference,
                ReviewFindingStatus::Open,
                &producing_pass,
                producing_run,
            )
            .is_none(),
            "producer inventory cannot include another pass's finding",
        );
    }

    /// INV-040: a terminal subject cannot acquire a later duplicate edge even
    /// to an otherwise eligible same-target same-policy finding.
    #[test]
    fn inv040_terminal_finding_cannot_acquire_reference() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let subject_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let canonical_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
        let subject = open_finding_with_producer(subject_reference, ReviewPolicy::version_one())
            .apply(finding_event(
                subject_reference,
                ReviewEventOrdinal::one(),
                succeeded_pass(ARBITRARY_JUDGMENT_PASS_SEED, ReviewPassKind::Judge),
                ReviewFindingEventKind::Rejected {
                    reason: text("not an issue"),
                },
            ))
            .expect("judgment may reject the open finding");
        let canonical =
            open_finding_with_producer(canonical_reference, ReviewPolicy::version_one());
        let canonical_evidence = ReviewReferencedFindingEvidence::try_from_finding(&canonical)
            .expect("open canonical finding is eligible reference evidence");
        let event = finding_event(
            subject_reference,
            ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
            succeeded_pass(ARBITRARY_DEDUPE_PASS_SEED, ReviewPassKind::Dedupe),
            ReviewFindingEventKind::Duplicate {
                canonical: canonical_evidence,
            },
        );

        let error = subject
            .apply(event)
            .expect_err("terminal finding cannot acquire a reference edge");

        assert_eq!(
            error.failure(),
            ReviewFindingTransitionFailure::InvalidTransition {
                current: ReviewFindingStatus::Rejected,
            },
        );
    }

    /// INV-040: complete reconstitution rejects a direct two-finding cycle
    /// even when each stored admission status was independently eligible.
    #[test]
    fn inv040_reconstituted_reference_graph_rejects_direct_cycle() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let first_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let second_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
        let first_open = open_finding_with_producer(first_reference, ReviewPolicy::version_one());
        let second_open = open_finding_with_producer(second_reference, ReviewPolicy::version_one());
        let first_evidence = ReviewReferencedFindingEvidence::try_from_finding(&first_open)
            .expect("first open finding is eligible reference evidence");
        let second_evidence = ReviewReferencedFindingEvidence::try_from_finding(&second_open)
            .expect("second open finding is eligible reference evidence");
        let first = ReviewFinding::try_reconstitute(
            first_open.proposal().clone(),
            vec![finding_event(
                first_reference,
                ReviewEventOrdinal::one(),
                succeeded_pass(ARBITRARY_DEDUPE_PASS_SEED, ReviewPassKind::Dedupe),
                ReviewFindingEventKind::Duplicate {
                    canonical: second_evidence,
                },
            )],
        )
        .expect("individual frozen edge is locally valid");
        let second = ReviewFinding::try_reconstitute(
            second_open.proposal().clone(),
            vec![finding_event(
                second_reference,
                ReviewEventOrdinal::one(),
                succeeded_pass(23, ReviewPassKind::Dedupe),
                ReviewFindingEventKind::Duplicate {
                    canonical: first_evidence,
                },
            )],
        )
        .expect("opposite frozen edge is locally valid");

        let error = validate_complete_review_finding_reference_graph(&[first, second])
            .expect_err("complete graph reconstitution must reject a direct cycle");

        assert_eq!(
            *error,
            ReviewFindingReferenceGraphError::Cycle {
                finding: second_reference,
                referenced: first_reference,
            },
        );
    }

    /// INV-040: complete reconstitution follows independent producer ancestry
    /// across runs and rejects a transitive three-finding cycle.
    #[test]
    fn inv040_reconstituted_reference_graph_rejects_transitive_cycle() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let first_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let second_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
        let third_reference = finding_ref_for_producer(target, 5);
        let first_open = open_finding_with_producer(first_reference, ReviewPolicy::version_one());
        let second_open = open_finding_with_producer(second_reference, ReviewPolicy::version_one());
        let third_open = open_finding_with_producer(third_reference, ReviewPolicy::version_one());
        let first_evidence = ReviewReferencedFindingEvidence::try_from_finding(&first_open)
            .expect("first open finding is eligible reference evidence");
        let second_evidence = ReviewReferencedFindingEvidence::try_from_finding(&second_open)
            .expect("second open finding is eligible reference evidence");
        let third_evidence = ReviewReferencedFindingEvidence::try_from_finding(&third_open)
            .expect("third open finding is eligible reference evidence");
        let first = ReviewFinding::try_reconstitute(
            first_open.proposal().clone(),
            vec![finding_event(
                first_reference,
                ReviewEventOrdinal::one(),
                succeeded_pass(ARBITRARY_DEDUPE_PASS_SEED, ReviewPassKind::Dedupe),
                ReviewFindingEventKind::Duplicate {
                    canonical: second_evidence,
                },
            )],
        )
        .expect("first frozen edge is locally valid");
        let second = ReviewFinding::try_reconstitute(
            second_open.proposal().clone(),
            vec![finding_event(
                second_reference,
                ReviewEventOrdinal::one(),
                succeeded_pass(23, ReviewPassKind::Dedupe),
                ReviewFindingEventKind::Duplicate {
                    canonical: third_evidence,
                },
            )],
        )
        .expect("second frozen edge is locally valid");
        let third = ReviewFinding::try_reconstitute(
            third_open.proposal().clone(),
            vec![finding_event(
                third_reference,
                ReviewEventOrdinal::one(),
                succeeded_pass(24, ReviewPassKind::Dedupe),
                ReviewFindingEventKind::Duplicate {
                    canonical: first_evidence,
                },
            )],
        )
        .expect("third frozen edge is locally valid");

        let error = validate_complete_review_finding_reference_graph(&[first, second, third])
            .expect_err("complete graph reconstitution must reject a transitive cycle");

        assert_eq!(
            *error,
            ReviewFindingReferenceGraphError::Cycle {
                finding: third_reference,
                referenced: first_reference,
            },
        );
    }

    /// INV-040: complete reconstitution rejects an edge whose independently
    /// authenticated referenced root is absent from the supplied target graph.
    #[test]
    fn inv040_reconstituted_reference_graph_rejects_missing_root() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let subject_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let canonical_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
        let subject_open =
            open_finding_with_producer(subject_reference, ReviewPolicy::version_one());
        let canonical =
            open_finding_with_producer(canonical_reference, ReviewPolicy::version_one());
        let canonical_evidence = ReviewReferencedFindingEvidence::try_from_finding(&canonical)
            .expect("canonical open finding is eligible reference evidence");
        let subject = ReviewFinding::try_reconstitute(
            subject_open.proposal().clone(),
            vec![finding_event(
                subject_reference,
                ReviewEventOrdinal::one(),
                succeeded_pass(ARBITRARY_DEDUPE_PASS_SEED, ReviewPassKind::Dedupe),
                ReviewFindingEventKind::Duplicate {
                    canonical: canonical_evidence,
                },
            )],
        )
        .expect("individual frozen edge is locally valid");

        let error = validate_complete_review_finding_reference_graph(&[subject])
            .expect_err("complete graph cannot omit a referenced root");

        assert_eq!(
            *error,
            ReviewFindingReferenceGraphError::MissingReferencedFinding {
                finding: subject_reference,
                referenced: canonical_reference,
            },
        );
    }

    /// INV-040: supersession admits the same authenticated cross-run boundary
    /// as duplicate classification without reparenting either finding.
    #[test]
    fn inv040_finding_admits_authenticated_cross_run_supersession() {
        let target = target_id(CANONICAL_TARGET_SEED);
        let subject_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
        let successor_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
        let subject = open_finding_with_producer(subject_reference, ReviewPolicy::version_one());
        let successor =
            open_finding_with_producer(successor_reference, ReviewPolicy::version_one());
        let successor_evidence = ReviewReferencedFindingEvidence::try_from_finding(&successor)
            .expect("open successor is eligible reference evidence");
        let event = finding_event(
            subject_reference,
            ReviewEventOrdinal::one(),
            succeeded_pass(ARBITRARY_DEDUPE_PASS_SEED, ReviewPassKind::Dedupe),
            ReviewFindingEventKind::Superseded {
                successor: successor_evidence,
            },
        );

        let superseded = subject
            .apply(event)
            .expect("equal-target equal-policy cross-run supersession is admitted");

        assert_eq!(superseded.status(), ReviewFindingStatus::Superseded);
        assert_eq!(
            successor_evidence.reference(),
            successor.proposal().reference()
        );
        assert_ne!(
            superseded.proposal().reference().pass(),
            successor.proposal().reference().pass(),
            "independent producer ancestry must be preserved",
        );
    }

    /// INV-040: referenced-finding reconstitution rejects a producer whose
    /// canonical operation is not successful read-only review.
    #[test]
    fn inv040_referenced_finding_reconstitution_rejects_incompatible_producer_kind() {
        let reference =
            finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
        let inventory = ReviewProducedFindings::try_new(vec![reference])
            .expect("fixture inventory is canonical");
        let judge_pass = ReviewPassEvidence::new(
            reference.pass(),
            ReviewPassKind::Judge,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(6),
                output_frontier: frontier_id(8),
                result: Some(ReviewPassResult::ProducedFindings(inventory.clone())),
            },
        );
        let review_pass = ReviewPassEvidence::new(
            reference.pass(),
            ReviewPassKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(6),
                output_frontier: frontier_id(8),
                result: Some(ReviewPassResult::ProducedFindings(inventory)),
            },
        );
        let judge_run = ReviewRunEvidence::new(
            reference.run(),
            ReviewWorkflowKind::JudgeFindings,
            ReviewPolicy::version_one(),
            ReviewRunState::Succeeded {
                concluding_pass: reference.pass(),
            },
        );

        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                reference,
                ReviewFindingStatus::Open,
                &judge_pass,
                judge_run,
            )
            .is_none(),
            "judge pass cannot authenticate finding production",
        );
        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                reference,
                ReviewFindingStatus::Open,
                &review_pass,
                judge_run,
            )
            .is_none(),
            "judge workflow cannot authenticate finding production",
        );
    }

    /// INV-040: referenced-finding reconstitution rejects incomplete or
    /// contradictory producer terminal evidence.
    #[test]
    fn inv040_referenced_finding_reconstitution_rejects_incomplete_producer_outcome() {
        let reference =
            finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
        let failed_pass = ReviewPassEvidence::new(
            reference.pass(),
            ReviewPassKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            ReviewPassState::Failed { turn: turn_id(6) },
        );
        let unsealed_pass = ReviewPassEvidence::new(
            reference.pass(),
            ReviewPassKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
            ReviewPassState::Succeeded {
                turn: turn_id(6),
                output_frontier: frontier_id(8),
                result: None,
            },
        );
        let failed_run = pass_run_evidence(&failed_pass);
        let unsealed_run = pass_run_evidence(&unsealed_pass);

        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                reference,
                ReviewFindingStatus::Open,
                &failed_pass,
                failed_run,
            )
            .is_none(),
            "failed producer cannot authenticate immutable finding content",
        );
        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                reference,
                ReviewFindingStatus::Open,
                &unsealed_pass,
                unsealed_run,
            )
            .is_none(),
            "successful producer without sealed inventory must fail closed",
        );
    }

    /// INV-040: referenced-finding reconstitution rejects a run that
    /// contradicts its producer's complete frozen policy.
    #[test]
    fn inv040_referenced_finding_reconstitution_rejects_producer_policy_mismatch() {
        let reference =
            finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
        let finding = open_finding_with_producer(reference, ReviewPolicy::version_one());
        let producing_pass = finding.proposal().producing_pass();
        let mismatched_run = ReviewRunEvidence::new(
            reference.run(),
            ReviewWorkflowKind::ReadOnlyReview,
            unsupported_policy(),
            ReviewRunState::Succeeded {
                concluding_pass: reference.pass(),
            },
        );

        assert!(
            ReviewReferencedFindingEvidence::try_reconstitute(
                reference,
                ReviewFindingStatus::Open,
                producing_pass,
                mismatched_run,
            )
            .is_none(),
            "producer pass and owning run must freeze one equal complete policy",
        );
    }

    /// INV-040: a complete graph cannot substitute the same aggregate root
    /// twice while claiming complete target coverage.
    #[test]
    fn inv040_reconstituted_reference_graph_rejects_duplicate_root() {
        let reference =
            finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
        let finding = open_finding_with_producer(reference, ReviewPolicy::version_one());

        let error = validate_complete_review_finding_reference_graph(&[finding.clone(), finding])
            .expect_err("complete graph cannot repeat a finding root");

        assert_eq!(
            *error,
            ReviewFindingReferenceGraphError::DuplicateFinding { reference },
        );
    }

    /// INV-040: a complete graph for one immutable target cannot include a
    /// disconnected root from another target.
    #[test]
    fn inv040_reconstituted_reference_graph_rejects_foreign_target_root() {
        let expected_target = target_id(CANONICAL_TARGET_SEED);
        let foreign_target = target_id(2);
        let expected_reference = finding_ref_for_producer(expected_target, PRODUCING_PASS_SEED);
        let foreign_reference = finding_ref_for_producer(foreign_target, REASSIGNED_PASS_SEED);
        let expected = open_finding_with_producer(expected_reference, ReviewPolicy::version_one());
        let foreign = open_finding_with_producer(foreign_reference, ReviewPolicy::version_one());

        let error = validate_complete_review_finding_reference_graph(&[expected, foreign])
            .expect_err("complete target graph cannot mix immutable targets");

        assert_eq!(
            *error,
            ReviewFindingReferenceGraphError::ForeignTargetRoot {
                expected: expected_target,
                actual: foreign_target,
            },
        );
    }

    #[test]
    fn review_finding_reference_graph_error_exposes_safe_standard_diagnostic() {
        let finding =
            finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
        let referenced =
            finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), REASSIGNED_PASS_SEED);
        let error = ReviewFindingReferenceGraphError::MissingReferencedFinding {
            finding,
            referenced,
        };

        let source = std::error::Error::source(&error);
        assert!(source.is_none());
        expect!["review finding ReviewFindingRef { pass: ReviewPassRef { run: ReviewRunRef { target: ReviewTargetId(00000000-0000-0000-0000-000000000001), run: ReviewRunId(00000000-0000-0000-0000-000000002713) }, pass: ReviewPassId(00000000-0000-0000-0000-000000000003) }, finding: ReviewFindingId(00000000-0000-0000-0000-000000004e23) } references missing finding ReviewFindingRef { pass: ReviewPassRef { run: ReviewRunRef { target: ReviewTargetId(00000000-0000-0000-0000-000000000001), run: ReviewRunId(00000000-0000-0000-0000-000000002725) }, pass: ReviewPassId(00000000-0000-0000-0000-000000000015) }, finding: ReviewFindingId(00000000-0000-0000-0000-000000004e35) }"]
        .assert_eq(&error.to_string());
    }
}
