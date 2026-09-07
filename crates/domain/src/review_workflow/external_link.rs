//! Review external link for `docs/spec/review-workflows.md`.

use crate::ReviewExternalLinkId;
use crate::ReviewPassId;
use crate::ReviewRunId;
use crate::ReviewTargetId;
use std::collections::BTreeMap;

use super::{
    ReviewEventOrdinal, ReviewFindingEventResultKind, ReviewFindingRef, ReviewKey,
    ReviewPassEvidence, ReviewPassKind, ReviewPassRef, ReviewPassResult, ReviewPassState,
    ReviewPassTurnOutcome, ReviewRunEvidence, ReviewRunRef, ReviewTarget, ReviewTargetSubject,
    finding_event_result_matches_pass, run_evidence_matches_pass,
};

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
    pub(super) pass_evidence: ReviewPassEvidence,
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
    pub(super) id: ReviewExternalLinkId,
    pub(super) association: ReviewExternalLinkAssociation,
    pub(super) provider: ReviewKey,
    pub(super) object_kind: ReviewExternalObjectKind,
    pub(super) attachment: Option<ReviewExternalLinkAttachment>,
    pub(super) observations: Vec<ReviewExternalLinkObservation>,
    pub(super) claims: Vec<ReviewExternalLinkClaim>,
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
