//! Shared review fixtures for `docs/spec/review-workflows.md`.

use super::finding::finding_transition;
use crate::AcceptedInputId;
use crate::ContextFrontierId;
use crate::ReviewExternalLinkId;
use crate::ReviewFindingId;
use crate::ReviewPassId;
use crate::ReviewRunId;
use crate::ReviewTargetId;
use crate::SessionId;
use crate::TurnId;

use super::*;
use uuid::Uuid;

/// Target identity repeated to close the ancestry cycle under test.
pub(super) const CYCLIC_ROOT_TARGET: u128 = 90;
/// Distinct intermediate target in the ancestry-cycle fixture.
pub(super) const CYCLIC_PARENT_TARGET: u128 = 91;
/// Arbitrary root revision; only exact continuity with the parent matters.
pub(super) const CYCLIC_ROOT_HEAD: &str = "cyclic-root-head";
/// Arbitrary parent revision; only exact continuity with the child matters.
pub(super) const CYCLIC_PARENT_HEAD: &str = "cyclic-parent-head";
/// Arbitrary proposed child revision in the rejected cycle.
pub(super) const CYCLIC_CHILD_HEAD: &str = "cyclic-child-head";
/// Canonical target shared by the ordinary review-workflow fixtures.
pub(super) const CANONICAL_TARGET_SEED: u128 = 1;
/// Producing pass used by the canonical finding fixture.
pub(super) const PRODUCING_PASS_SEED: u128 = 3;
/// Namespace separating pass-derived run identities from pass identities.
const PASS_RUN_NAMESPACE: u128 = 10_000;
/// Canonical finding used by ordinary finding-history fixtures.
pub(super) const CANONICAL_FINDING_SEED: u128 = 10;
/// Distinct finding used to exercise foreign-owner rejection.
pub(super) const FOREIGN_FINDING_SEED: u128 = 11;
/// Arbitrary judgment pass used by unchanged-aggregate fixtures.
pub(super) const ARBITRARY_JUDGMENT_PASS_SEED: u128 = 20;
/// Distinct pass used to exercise changed user-global pass claims.
pub(super) const REASSIGNED_PASS_SEED: u128 = 21;
/// Distinct event pass used by referenced-finding fixtures.
pub(super) const ARBITRARY_DEDUPE_PASS_SEED: u128 = 22;
/// Arbitrary turn retained by deliberately cross-wired pass evidence.
pub(super) const CROSS_WIRED_TURN_SEED: u128 = 121;
/// Arbitrary external-link reservation used by transition fixtures.
pub(super) const ARBITRARY_LINK_SEED: u128 = 30;

/// Maps an arbitrary seed to a deterministic target identity; equality and
/// difference between seeds are the only default semantics.
pub(super) fn target_id(value: u128) -> ReviewTargetId {
    ReviewTargetId::from_uuid(Uuid::from_u128(value))
}

/// Maps an arbitrary seed to a deterministic run identity; equality and
/// difference between seeds are the only default semantics.
pub(super) fn run_id(value: u128) -> ReviewRunId {
    ReviewRunId::from_uuid(Uuid::from_u128(value))
}

/// Maps an arbitrary seed to a deterministic pass identity; equality and
/// difference between seeds are the only default semantics.
pub(super) fn pass_id(value: u128) -> ReviewPassId {
    ReviewPassId::from_uuid(Uuid::from_u128(value))
}

/// Maps an arbitrary seed to a deterministic finding identity; equality
/// and difference between seeds are the only default semantics.
pub(super) fn finding_id(value: u128) -> ReviewFindingId {
    ReviewFindingId::from_uuid(Uuid::from_u128(value))
}

/// Maps an arbitrary seed to a deterministic external-link identity;
/// equality and difference between seeds are the only default semantics.
pub(super) fn link_id(value: u128) -> ReviewExternalLinkId {
    ReviewExternalLinkId::from_uuid(Uuid::from_u128(value))
}

/// Maps an arbitrary seed to a deterministic session identity; equality
/// and difference between seeds are the only default semantics.
pub(super) fn session_id(value: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(value))
}

/// Maps an arbitrary seed to a deterministic accepted-input identity;
/// equality and difference between seeds are the only default semantics.
pub(super) fn accepted_input_id(value: u128) -> AcceptedInputId {
    AcceptedInputId::from_uuid(Uuid::from_u128(value))
}

/// Maps an arbitrary seed to a deterministic turn identity; equality and
/// difference between seeds are the only default semantics.
pub(super) fn turn_id(value: u128) -> TurnId {
    TurnId::from_uuid(Uuid::from_u128(value))
}

/// Maps an arbitrary seed to a deterministic frontier identity; equality
/// and difference between seeds are the only default semantics.
pub(super) fn frontier_id(value: u128) -> ContextFrontierId {
    ContextFrontierId::from_uuid(Uuid::from_u128(value))
}

/// Admits exact fixture key content; repeated and distinct strings model
/// key equality and difference unless a test asserts the content.
pub(super) fn key(value: &str) -> ReviewKey {
    ReviewKey::try_new(String::from(value)).expect("fixture key is valid")
}

/// Admits exact fixture narrative content; repeated and distinct strings
/// model text equality and difference unless a test asserts the content.
pub(super) fn text(value: &str) -> ReviewText {
    ReviewText::try_new(String::from(value)).expect("fixture text is valid")
}

pub(super) fn run_ref() -> ReviewRunRef {
    pass_ref(PRODUCING_PASS_SEED).run()
}

pub(super) fn run_evidence() -> ReviewRunEvidence {
    ReviewRunEvidence::new(
        run_ref(),
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
        ReviewRunState::Succeeded {
            concluding_pass: pass_ref(PRODUCING_PASS_SEED),
        },
    )
}

pub(super) fn unsupported_policy() -> ReviewPolicy {
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
pub(super) fn pass_ref(value: u128) -> ReviewPassRef {
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
pub(super) fn succeeded_pass(value: u128, kind: ReviewPassKind) -> ReviewPassEvidence {
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
pub(super) fn blocked_pass(value: u128, kind: ReviewPassKind) -> ReviewPassEvidence {
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

pub(super) fn produced_findings_pass(
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

pub(super) fn pass_run_evidence(pass: &ReviewPassEvidence) -> ReviewRunEvidence {
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

pub(super) fn pass_with_finding_event(
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

pub(super) fn pass_with_attachment_result(
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

pub(super) fn pass_with_posted_attachment_result(
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
                ReviewExternalLinkAttachmentResult::new(link, external_object.clone(), Some(event)),
            )),
        },
        other => other.clone(),
    };
    ReviewPassEvidence::new(pass.reference(), pass.kind(), pass.policy(), state)
}

pub(super) fn finding_event(
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

pub(super) fn posted_finding_event(
    finding: ReviewFindingRef,
    ordinal: ReviewEventOrdinal,
    pass: ReviewPassEvidence,
    link: ReviewExternalLinkId,
) -> ReviewFindingEvent {
    let external_object = key("external-comment-42");
    let pass = pass_with_posted_attachment_result(finding, ordinal, pass, link, &external_object);
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
pub(super) fn referenced_finding(
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
pub(super) fn finding_ref(value: u128) -> ReviewFindingRef {
    ReviewFindingRef::new(pass_ref(3), finding_id(value))
}

pub(super) fn target_with_base() -> ReviewTarget {
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

pub(super) fn target_without_base() -> ReviewTarget {
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

pub(super) struct FindingConfidenceAxes {
    pub(super) is_real: u16,
    pub(super) severity_label: u16,
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

pub(super) fn finding_content(diff_side: Option<ReviewFindingDiffSide>) -> ReviewFindingContent {
    finding_content_with_confidence_axes(
        diff_side,
        FindingConfidenceAxes {
            is_real: 8_500,
            severity_label: 8_500,
        },
    )
}

pub(super) fn proposal_with_confidence_axes(
    confidence: FindingConfidenceAxes,
) -> ReviewFindingProposal {
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

pub(super) fn proposal() -> ReviewFindingProposal {
    proposal_with_confidence_axes(FindingConfidenceAxes {
        is_real: 8_500,
        severity_label: 8_500,
    })
}

pub(super) fn finding_ref_for_producer(
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

pub(super) fn open_finding_with_producer(
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
pub(super) fn succeeded_review_pass() -> ReviewPass {
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
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
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

pub(super) fn succeeded_judgment_pass() -> ReviewPass {
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

pub(super) fn accepted_finding_result() -> ReviewPassResult {
    ReviewPassResult::FindingEvent(ReviewFindingEventResult::new(
        finding_ref(10),
        ReviewEventOrdinal::one(),
        ReviewFindingEventResultKind::Accepted {
            confidence: crate::ReviewJudgeConfidence::try_new(5).expect("judge confidence"),
        },
    ))
}

/// Labeled target fixture whose values are arbitrary except where repeated
/// or distinguished to exercise logical-target continuity.
pub(super) struct ChangeRequestTargetFixture<'a> {
    pub(super) target_seed: u128,
    pub(super) provider: &'a str,
    pub(super) repository: &'a str,
    pub(super) number: u64,
    pub(super) head: &'a str,
}

pub(super) fn change_request_target(fixture: ChangeRequestTargetFixture<'_>) -> ReviewTarget {
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
pub(super) struct AttachedTargetLinkFixture<'a> {
    pub(super) link_seed: u128,
    pub(super) run_seed: u128,
    pub(super) pass_seed: u128,
    pub(super) external_object: &'a str,
}

pub(super) fn attached_target_link(
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

pub(super) fn attached_finding_link(
    finding: ReviewFindingRef,
    link: ReviewExternalLinkId,
) -> ReviewExternalLink {
    attached_finding_link_with_pass(finding, link, succeeded_pass(20, ReviewPassKind::Publish))
}

pub(super) fn pending_link(
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

pub(super) fn reconstitute_link(link: &ReviewExternalLink) -> ReviewExternalLink {
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

pub(super) fn attachment_evidence(
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

pub(super) fn observation_evidence(
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

pub(super) fn no_change_pass(
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

pub(super) fn publication_blocked_pass(
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

pub(super) fn attached_finding_link_with_pass(
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

pub(super) fn finding_link_ref(
    finding: ReviewFindingRef,
    link: ReviewExternalLinkId,
) -> ReviewFindingExternalLinkRef {
    ReviewFindingExternalLinkRef::try_new(finding, &attached_finding_link(finding, link))
        .expect("fixture link is attached to the exact finding")
}

pub(super) fn pending_finding_link_ref(
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
pub(super) fn over_budget_finding_inventory() -> Vec<ReviewFindingRef> {
    (1..=(REVIEW_PRODUCED_FINDINGS_MAXIMUM as u128 + 1))
        .map(|value| ReviewFindingRef::new(pass_ref(PRODUCING_PASS_SEED), finding_id(value)))
        .collect()
}

#[track_caller]
pub(super) fn assert_pass_reconstitution_rejects(
    input: ReviewPassReconstitutionInput,
    expected: ReviewPassReconstitutionFailure,
) {
    let error = ReviewPass::try_reconstitute(input.clone())
        .expect_err("cross-wired canonical pass evidence must fail closed");
    assert_eq!(error.failure(), expected);
    assert_eq!(*error.input(), input);
}

#[track_caller]
pub(super) fn assert_pass_outcome_reconstitutes(
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

#[derive(Debug, serde::Serialize)]
pub(super) struct FindingTransitionRow {
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

pub(super) fn finding_transition_rows() -> Vec<FindingTransitionRow> {
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
            ReviewFindingEventKind::Accepted {
                confidence: crate::ReviewJudgeConfidence::try_new(5).expect("judge confidence"),
            },
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
                    let previous = (current == ReviewFindingStatus::BlockedWithReason).then(|| {
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
