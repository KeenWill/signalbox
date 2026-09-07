//! Review aggregate tests for `docs/spec/review-workflows.md`.

use super::test_support::{
    ARBITRARY_DEDUPE_PASS_SEED, ARBITRARY_JUDGMENT_PASS_SEED, ARBITRARY_LINK_SEED,
    AttachedTargetLinkFixture, CANONICAL_FINDING_SEED, CANONICAL_TARGET_SEED,
    CROSS_WIRED_TURN_SEED, CYCLIC_CHILD_HEAD, CYCLIC_PARENT_HEAD, CYCLIC_PARENT_TARGET,
    CYCLIC_ROOT_HEAD, CYCLIC_ROOT_TARGET, ChangeRequestTargetFixture, FOREIGN_FINDING_SEED,
    FindingConfidenceAxes, PRODUCING_PASS_SEED, REASSIGNED_PASS_SEED, accepted_finding_result,
    accepted_input_id, assert_pass_outcome_reconstitutes, assert_pass_reconstitution_rejects,
    attached_finding_link, attached_finding_link_with_pass, attached_target_link,
    attachment_evidence, blocked_pass, change_request_target, finding_content, finding_event,
    finding_id, finding_link_ref, finding_ref, finding_ref_for_producer, finding_transition_rows,
    frontier_id, key, link_id, no_change_pass, observation_evidence, open_finding_with_producer,
    over_budget_finding_inventory, pass_id, pass_ref, pass_run_evidence,
    pass_with_attachment_result, pass_with_finding_event, pass_with_posted_attachment_result,
    pending_finding_link_ref, pending_link, posted_finding_event, produced_findings_pass, proposal,
    proposal_with_confidence_axes, publication_blocked_pass, reconstitute_link, referenced_finding,
    run_evidence, run_id, run_ref, session_id, succeeded_judgment_pass, succeeded_pass,
    succeeded_review_pass, target_id, target_with_base, target_without_base, text, turn_id,
    unsupported_policy,
};
use super::*;
use expect_test::expect;
use expectable::print;

/// change-request review freezes its comparison revision.
#[test]
fn change_request_target_requires_frozen_base_revision() {
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

/// a stack parent remains inside the child's exact repository
/// topology.
#[test]
fn review_target_rejects_foreign_stack_parent() {
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

/// reconstitution preserves the child row's exact parent
/// identity instead of adopting a compatible joined target.
#[test]
fn review_target_reconstitution_rejects_substituted_parent_identity() {
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

/// a parent edge requires an exact frozen child comparison.
#[test]
fn review_target_rejects_parent_without_base_revision() {
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

/// the child's comparison revision is the canonical parent head.
#[test]
fn review_target_rejects_revision_disconnected_stack_parent() {
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

/// complete stack ancestry cannot repeat the target being
/// constructed.
#[test]
fn review_target_rejects_transitive_parent_cycle() {
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

/// refresh history for one change request is not an immediate
/// stack edge.
#[test]
fn review_target_rejects_immediate_same_change_request_parent() {
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

/// refresh history for one change request cannot reappear
/// transitively in a stack chain.
#[test]
fn review_target_rejects_transitive_same_change_request_parent() {
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

/// a valid parent reference retains canonical scope and head
/// evidence from the supplied snapshot.
#[test]
fn review_target_derives_parent_evidence_from_canonical_snapshot() {
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

/// review identities remain distinct while composite references
/// preserve exact ancestry.
#[test]
fn review_references_preserve_typed_identity_ancestry() {
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

/// diff-relative locations require a frozen comparison.
#[test]
fn diff_relative_finding_requires_target_comparison_revision() {
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

/// standalone commit review may remain file-relative.
#[test]
fn file_relative_finding_allows_standalone_commit_target() {
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

/// a finding reference authenticates its exact producing pass, not
/// merely another pass in the same run.
#[test]
fn finding_reference_rejects_cross_wired_producing_pass() {
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

/// immutable finding content requires a canonically successful
/// read-only-review producer.
#[test]
fn finding_proposal_rejects_incompatible_producing_pass() {
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

/// the finding producer's policy must come from its exact
/// independently loaded run.
#[test]
fn finding_proposal_rejects_foreign_producing_run_policy() {
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

/// a finding cannot be produced while its canonical run remains
/// active.
#[test]
fn finding_proposal_rejects_contradictory_run_lifecycle() {
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

/// a producing run must name the exact pass that produced the
/// finding inventory.
#[test]
fn finding_proposal_rejects_run_naming_another_pass() {
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

/// run transitions reject a pass owned by another run.
#[test]
fn run_transition_rejects_foreign_pass() {
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

/// a run admits only the pass kind corresponding to its workflow.
#[test]
fn run_transition_rejects_incompatible_pass_kind() {
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

/// a transition cannot introduce a pass that was never admitted
/// through queued-pass construction.
#[test]
fn run_transition_rejects_unrecorded_pass() {
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

/// queued cancellation retains an already-recorded pass and its
/// canonical pre-start cancellation.
#[test]
fn queued_run_cancellation_retains_cancelled_pass() {
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
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
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

/// cancellation after activation must retain the exact turn that
/// was cancelled.
#[test]
fn running_run_rejects_turnless_pass_cancellation() {
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
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
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

/// pass construction records its identity on the run, so later
/// cancellation cannot claim that no pass existed.
#[test]
fn queued_run_rejects_passless_cancellation_after_pass_construction() {
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

/// queued pass construction authenticates its accepted-input
/// session.
#[test]
fn pass_construction_rejects_foreign_accepted_input_session() {
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

/// queued-pass construction authenticates its parent workflow.
#[test]
fn pass_construction_rejects_incompatible_run_workflow() {
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
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
    )
    .expect_err("pass kind must correspond to its canonical run workflow");
    assert_eq!(
        error.failure(),
        ReviewPassConstructionFailure::RunWorkflowMismatch
    );
}

/// queued-pass construction takes workflow evidence only from the
/// pass's exact parent run.
#[test]
fn pass_construction_rejects_foreign_run_evidence() {
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
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
    )
    .expect_err("workflow evidence must come from the pass's exact run");

    assert_eq!(error.failure(), ReviewPassConstructionFailure::ForeignRun);
    assert_eq!(
        error.run_evidence().reference(),
        ReviewRunRef::new(target_id(1), run_id(99))
    );
}

/// one active pass turn remains fixed through terminalization.
#[test]
fn pass_transition_rejects_changed_turn() {
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
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
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

/// a running pass admits monotonic lag after its canonical turn terminalizes.
#[test]
fn running_pass_admits_terminal_turn_projection_lag() {
    let lagging = ReviewPassState::Running { turn: turn_id(6) };
    let input = ReviewPassReconstitutionInput::new(
        pass_ref(3),
        ReviewPassKind::ReadOnlyReview,
        run_ref(),
        ReviewWorkflowKind::ReadOnlyReview,
        session_id(4),
        accepted_input_id(5),
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
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

/// a queued pass starts only while its canonical turn is active; terminal outcomes cannot lead
/// an unprojected start.
#[test]
fn queued_pass_start_requires_active_turn() {
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
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
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

/// run reconstitution accepts its exact canonical pass outcome.
#[test]
fn run_reconstitution_accepts_exact_pass_outcome() {
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

/// run reconstitution rejects a contradictory canonical pass outcome.
#[test]
fn run_reconstitution_rejects_cross_wired_pass_outcome() {
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

/// canonical pass evidence must carry the run's frozen policy.
#[test]
fn run_reconstitution_rejects_foreign_pass_policy() {
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

/// the workflow discriminator must come from the pass's own run
/// row.
#[test]
fn pass_reconstitution_rejects_foreign_workflow_run() {
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

/// a run that names a pass requires independently loaded canonical pass evidence.
#[test]
fn run_reconstitution_requires_pass_evidence() {
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

/// exact canonical accepted-input, turn, and frontier evidence
/// reconstitutes the stored pass state.
#[test]
fn pass_reconstitution_accepts_exact_canonical_evidence() {
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
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
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

/// pass reconstitution preserves the pass row's accepted-input
/// identity instead of adopting the joined evidence identity.
#[test]
fn pass_reconstitution_rejects_substituted_accepted_input_identity() {
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

/// the accepted input must belong to the pass session.
#[test]
fn pass_reconstitution_rejects_foreign_accepted_input_session() {
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

/// a turn-naming pass state requires its canonical turn row.
#[test]
fn pass_reconstitution_rejects_missing_turn_evidence() {
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

/// a queued pass admits no turn evidence.
#[test]
fn pass_reconstitution_rejects_unexpected_turn_evidence() {
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

/// the canonical turn row must name the pass state's exact turn.
#[test]
fn pass_reconstitution_rejects_foreign_turn() {
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

/// the canonical turn must belong to the pass session.
#[test]
fn pass_reconstitution_rejects_foreign_turn_session() {
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

/// the canonical turn must originate from the pass input.
#[test]
fn pass_reconstitution_rejects_foreign_turn_origin_input() {
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

/// a succeeded pass rejects a contradictory canonical outcome.
#[test]
fn pass_reconstitution_rejects_mismatched_turn_outcome() {
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

/// the successful output must be the canonical terminal frontier.
#[test]
fn pass_reconstitution_rejects_mismatched_output_frontier() {
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

/// a running pass accepts an active canonical turn.
#[test]
fn running_pass_accepts_active_turn_outcome() {
    assert_pass_outcome_reconstitutes(
        ReviewPassState::Running { turn: turn_id(6) },
        ReviewPassTurnOutcome::Active,
        None,
    );
}

/// a failed pass may project completed execution whose workflow result was invalid.
#[test]
fn failed_pass_accepts_completed_turn_outcome() {
    assert_pass_outcome_reconstitutes(
        ReviewPassState::Failed { turn: turn_id(6) },
        ReviewPassTurnOutcome::Completed,
        Some(frontier_id(8)),
    );
}

/// a failed pass accepts a failed canonical turn.
#[test]
fn failed_pass_accepts_failed_turn_outcome() {
    assert_pass_outcome_reconstitutes(
        ReviewPassState::Failed { turn: turn_id(6) },
        ReviewPassTurnOutcome::Failed,
        Some(frontier_id(8)),
    );
}

/// a failed pass accepts a refused canonical turn.
#[test]
fn failed_pass_accepts_refused_turn_outcome() {
    assert_pass_outcome_reconstitutes(
        ReviewPassState::Failed { turn: turn_id(6) },
        ReviewPassTurnOutcome::Refused,
        Some(frontier_id(8)),
    );
}

/// a blocked pass accepts a reconciliation-required turn.
#[test]
fn blocked_pass_accepts_reconciliation_turn_outcome() {
    assert_pass_outcome_reconstitutes(
        ReviewPassState::Blocked {
            turn: turn_id(6),
            result: None,
        },
        ReviewPassTurnOutcome::ReconciliationRequired,
        Some(frontier_id(8)),
    );
}

/// a post-start cancelled pass accepts a cancelled turn.
#[test]
fn cancelled_pass_accepts_cancelled_turn_outcome() {
    assert_pass_outcome_reconstitutes(
        ReviewPassState::Cancelled {
            turn: Some(turn_id(6)),
        },
        ReviewPassTurnOutcome::Cancelled,
        Some(frontier_id(8)),
    );
}

/// authenticated successful pass evidence may project one exact effect result without changing
/// its execution facts.
#[test]
fn succeeded_pass_evidence_projects_one_exact_effect_result() {
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

/// an authenticated effect projection admits exact replay but cannot be rebound to a distinct
/// result.
#[test]
fn pass_evidence_result_projection_is_immutable() {
    let pass = succeeded_pass(71, ReviewPassKind::ImportExternalContext);
    let result = ReviewPassResult::ExternalLinkNoChange(ReviewExternalLinkNoChangeResult::new(
        link_id(72),
        ReviewEventOrdinal::one(),
        ReviewExternalObjectState::Current,
    ));
    let projected = pass
        .project_result(result.clone())
        .expect("result-free succeeded evidence admits its first result");
    let distinct = ReviewPassResult::ExternalLinkNoChange(ReviewExternalLinkNoChangeResult::new(
        link_id(72),
        ReviewEventOrdinal::one(),
        ReviewExternalObjectState::Outdated,
    ));

    assert_eq!(projected.project_result(result), Some(projected.clone()));
    assert_eq!(projected.project_result(distinct), None);
}

/// authenticated blocked pass evidence may project one exact effect result without changing its
/// execution facts.
#[test]
fn blocked_pass_evidence_projects_one_exact_effect_result() {
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

/// non-effect pass states cannot project effect results.
#[test]
fn non_effect_pass_evidence_rejects_result_projection() {
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

/// a terminal canonical turn outcome always carries its checked terminal frontier.
#[test]
fn pass_evidence_rejects_terminal_outcome_without_frontier() {
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

/// an active canonical turn outcome never carries a terminal frontier.
#[test]
fn pass_evidence_rejects_active_outcome_with_frontier() {
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

/// a pending reservation is not posting evidence.
#[test]
fn posted_link_rejects_pending_reservation() {
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

/// a publication block binds one exact pending
/// reservation associated with the finding.
#[test]
fn blocked_link_accepts_exact_pending_reservation() {
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

/// an already attached object cannot be recorded as a
/// pending publication attempt.
#[test]
fn blocked_link_rejects_attached_reservation() {
    let finding = finding_ref(10);
    let attached = attached_finding_link(finding, link_id(30));

    let error = ReviewFindingPendingExternalLinkRef::try_new(finding, &attached)
        .expect_err("attached reservation is no longer pending");

    assert_eq!(
        error.failure(),
        ReviewFindingExternalLinkFailure::AlreadyAttached
    );
}

/// an attached link canonically associated with another
/// finding is not posting evidence for this finding.
#[test]
fn posted_link_rejects_foreign_canonical_association() {
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

/// a repository correlation that carries no review
/// content cannot prove that a finding was posted.
#[test]
fn posted_link_rejects_non_review_external_object() {
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

/// a posted finding consumes an attached canonical link
/// associated with that exact finding.
#[test]
fn posted_link_accepts_exact_attached_association() {
    let finding = finding_ref(10);
    let exact = attached_finding_link(finding, link_id(32));

    let posted = ReviewFindingExternalLinkRef::try_new(finding, &exact)
        .expect("attached canonical association supports posting");

    assert_eq!(posted.link(), link_id(32));
}

/// a terminal finding cannot reopen.
#[test]
fn finding_machine_rejects_terminal_reopening() {
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

/// finding history begins at ordinal one.
#[test]
fn finding_history_rejects_noncontiguous_first_ordinal() {
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

/// an event cannot be replayed into another same-run finding.
#[test]
fn finding_history_rejects_foreign_event_owner() {
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

/// finding-event reconstitution preserves the event row's exact
/// pass identity instead of adopting compatible joined pass evidence.
#[test]
fn finding_history_rejects_substituted_event_pass_identity() {
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

/// a referenced finding naming the aggregate's own identity is a
/// self-reference even when its producing-pass ancestry is cross-wired.
#[test]
fn finding_history_rejects_identity_self_reference() {
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

/// a same-target pass cannot produce an event outside its closed
/// pass-kind responsibility.
#[test]
fn finding_history_rejects_incompatible_event_pass_kind() {
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

/// every event pass uses the exact policy frozen by the finding's
/// producing run.
#[test]
fn finding_history_rejects_event_policy_mismatch() {
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

/// an event pass is authenticated against its exact canonical run
/// workflow rather than a copied compatible tuple.
#[test]
fn finding_history_rejects_cross_wired_event_run() {
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

/// a successful finding event pass cannot be paired with a failed
/// canonical run projection.
#[test]
fn finding_history_rejects_event_run_lifecycle_mismatch() {
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

/// a pass result must name the event's exact finding, ordinal, and
/// discriminator.
#[test]
fn finding_history_rejects_mismatched_pass_result() {
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

/// canonical and successor references admit only findings that do
/// not already carry a terminal reference edge.
#[test]
fn finding_history_rejects_ineligible_reference() {
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

/// reconstitution preserves the status frozen when a reference
/// was admitted, independently of the referenced finding's current state.
#[test]
fn referenced_finding_evidence_reconstitutes_only_admissible_status() {
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

/// one canonical pass identity cannot change outcome evidence
/// between events in the same complete finding history.
#[test]
fn finding_history_rejects_conflicting_reused_pass_evidence() {
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

/// a user-global pass identity cannot move to another run inside
/// one finding history.
#[test]
fn finding_history_rejects_reparented_pass_identity() {
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

/// a user-global run identity cannot change its one pass or
/// workflow inside one finding history.
#[test]
fn finding_history_rejects_changed_run_claim() {
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

/// complete history replay maintains indexed pass, run, and
/// publication claims instead of rescanning the growing event vector.
#[test]
fn finding_history_indexes_replay_claims() {
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

/// a compatible pass kind cannot author a finding event after a
/// failed outcome.
#[test]
fn finding_history_rejects_incompatible_event_pass_outcome() {
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

/// publication is attributed to the pass that produced
/// the attached external object.
#[test]
fn posted_event_rejects_another_publication_pass() {
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

/// a no-write import pass may reconcile a
/// publication-blocked finding after attaching the external object.
#[test]
fn publication_blocked_finding_can_reconcile_to_posted() {
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

/// reconciliation cannot substitute another pending
/// reservation for the publication attempt that blocked.
#[test]
fn publication_reconciliation_rejects_another_reservation() {
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

/// reconciliation cannot replay attachment evidence
/// consumed by an earlier posted event.
#[test]
fn reposting_rejects_consumed_publication_link() {
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

/// the complete nine-state finding transition surface stays
/// closed and reviewable as one table.
#[test]
fn finding_transition_matrix_is_closed() {
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
    .assert_eq(&print(&finding_transition_rows()));
}

/// a repair-blocked finding cannot cross the publication-only
/// reconciliation edge.
#[test]
fn repair_blocked_finding_rejects_posting() {
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

/// reservation leaves the external effect explicitly pending.
#[test]
fn external_link_reservation_is_pending() {
    let association = ReviewExternalLinkAssociation::Finding(finding_ref(10));
    let pending = pending_link(
        link_id(30),
        association,
        ReviewExternalObjectKind::ReviewComment,
    );
    assert!(pending.attachment().is_none());
    assert!(pending.observations().is_empty());
}

/// an external-state observation cannot precede attachment.
#[test]
fn external_link_rejects_observation_before_attachment() {
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

/// an attached link admits its first contiguous observation.
#[test]
fn attached_external_link_accepts_first_observation() {
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

/// failed or operation-incompatible passes cannot attach external
/// effect evidence.
#[test]
fn external_link_rejects_incompatible_attachment_pass() {
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

/// attachment evidence rejects a nested non-posted
/// finding event.
#[test]
fn attachment_rejects_nested_non_posted_event() {
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

/// attachment evidence rejects a nested posted event
/// that names another reservation.
#[test]
fn attachment_rejects_nested_foreign_reservation() {
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

/// external-state observations require successful import evidence.
#[test]
fn external_link_rejects_incompatible_observation_pass() {
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

/// unchanged polling does not create another durable observation.
#[test]
fn external_link_rejects_unchanged_observation() {
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

/// an unchanged external report consumes its import
/// pass without growing observation history.
#[test]
fn external_link_accepts_exact_no_change_result() {
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

/// reconstitution validates an unchanged claim against
/// its recorded historical frontier after a later changed observation.
#[test]
fn external_link_reconstitutes_historical_no_change_frontier() {
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

/// a no-change claim authenticates the exact latest
/// observation, not an equal historical state.
#[test]
fn external_link_rejects_stale_no_change_frontier() {
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

/// an unchanged report also retains its owning run
/// claim against a later pass substitution.
#[test]
fn external_link_no_change_retains_run_claim() {
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

/// a no-change result cannot name a different
/// reservation.
#[test]
fn external_link_rejects_foreign_no_change_result() {
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

/// an observation-producing pass cannot later be
/// reinterpreted as an unchanged report.
#[test]
fn external_link_rejects_reused_no_change_pass() {
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

/// a target-associated pending reservation can consume
/// one blocked publication pass.
#[test]
fn external_link_accepts_target_publication_block() {
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

/// a finding-associated pending reservation accepts
/// the blocked lifecycle event committed by its publication pass.
#[test]
fn external_link_accepts_finding_publication_block() {
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

/// a blocked publication cannot name another pending
/// reservation.
#[test]
fn external_link_rejects_foreign_publication_block() {
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

/// an attachment row retains its independently stored
/// producing-pass identity.
#[test]
fn external_link_rejects_substituted_attachment_pass_identity() {
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

/// an observation row retains its independently stored
/// observing-pass identity.
#[test]
fn external_link_rejects_substituted_observation_pass_identity() {
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

/// attachment evidence must be joined to the exact canonical run
/// that owns its pass.
#[test]
fn external_link_rejects_cross_wired_attachment_run() {
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

/// a successful attachment pass cannot be paired with a
/// failed canonical run projection.
#[test]
fn external_link_rejects_attachment_run_lifecycle_mismatch() {
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

/// observation evidence must be joined to the exact canonical run
/// that owns its pass.
#[test]
fn external_link_rejects_cross_wired_observation_run() {
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

/// a successful observation pass cannot be paired with
/// a still-running canonical run projection.
#[test]
fn external_link_rejects_observation_run_lifecycle_mismatch() {
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

/// a pass identity reused across observations cannot change its
/// terminal evidence.
#[test]
fn external_link_rejects_conflicting_reused_observation_pass() {
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

/// a user-global external-effect pass cannot move to another
/// run.
#[test]
fn external_link_rejects_reparented_pass_identity() {
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

/// an external-effect run cannot change its one pass or workflow.
#[test]
fn external_link_rejects_changed_run_claim() {
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

/// an observation from another same-target link cannot be
/// attributed to the loaded aggregate.
#[test]
fn external_link_rejects_foreign_observation_owner() {
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

/// an attachment from another same-target reservation cannot be
/// attributed to the loaded aggregate.
#[test]
fn external_link_rejects_foreign_attachment_owner() {
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

/// produced-finding inventories have one canonical identity
/// order.
#[test]
fn produced_findings_canonicalize_identity_order() {
    let first = finding_ref(10);
    let second = finding_ref(11);
    let inventory = ReviewProducedFindings::try_new(vec![second, first])
        .expect("distinct finding identities are canonicalizable");

    assert_eq!(inventory.findings(), &[first, second]);
}

/// a produced-finding inventory cannot repeat an identity.
#[test]
fn produced_findings_reject_duplicate_identity() {
    let finding = finding_ref(10);
    let error = ReviewProducedFindings::try_new(vec![finding, finding])
        .expect_err("one result cannot repeat a finding identity");

    assert_eq!(error, ReviewProducedFindingsError::Duplicate { finding });
}

/// a produced-finding inventory is bounded to 32 identities.
#[test]
fn produced_findings_reject_over_budget_inventory() {
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

/// a finding must appear in its exact producing pass inventory.
#[test]
fn finding_proposal_rejects_omitted_producing_result() {
    let empty = ReviewPassEvidence::new(
        pass_ref(3),
        ReviewPassKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
        ReviewPassState::Succeeded {
            turn: turn_id(103),
            output_frontier: frontier_id(203),
            result: Some(ReviewPassResult::ProducedFindings(
                ReviewProducedFindings::try_new(Vec::new()).expect("empty inventory is canonical"),
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

/// a proposal rejects a result inventory containing a finding
/// attributed to another producing pass.
#[test]
fn finding_proposal_rejects_cross_wired_producing_inventory() {
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

/// a durably reconstituted completed review always carries its
/// exact produced-finding inventory, including the empty inventory.
#[test]
fn read_only_pass_reconstitution_requires_produced_findings() {
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

/// a pass result cannot name a finding from another target.
#[test]
fn pass_reconstitution_rejects_foreign_result_target() {
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

/// a terminal pass may bind its exact result once.
#[test]
fn terminal_pass_binds_absent_result() {
    let result = accepted_finding_result();
    let bound = succeeded_judgment_pass()
        .bind_result(result.clone())
        .expect("compatible exact result binds");

    assert_eq!(bound.state().result(), Some(&result));
}

/// replaying the equal bound pass result is idempotent.
#[test]
fn terminal_pass_accepts_equal_result_replay() {
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

/// a distinct result cannot replace an already bound result.
#[test]
fn terminal_pass_rejects_distinct_result_rebind() {
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

/// a rejected run transition returns its unchanged aggregate.
#[test]
fn rejected_run_transition_retains_current_aggregate() {
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

/// a rejected pass transition returns its unchanged aggregate.
#[test]
fn rejected_pass_transition_retains_current_aggregate() {
    let current = succeeded_review_pass();
    let error = current
        .clone()
        .transition(current.state().clone(), None)
        .expect_err("a terminal pass cannot transition again");

    assert_eq!(error.current(), &current);
}

/// a rejected finding event returns its unchanged aggregate.
#[test]
fn rejected_finding_transition_retains_current_aggregate() {
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

/// a rejected link transition returns its unchanged aggregate.
#[test]
fn rejected_link_transition_retains_current_aggregate() {
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

/// read-only success admission includes its exact finding inventory.
#[test]
fn read_only_success_transition_requires_produced_findings() {
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

/// a blocked publication transition must consume one exact
/// reservation-bearing result.
#[test]
fn publish_block_transition_requires_result() {
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
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
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

/// reconstitution also fails closed when a blocked publication
/// row omits its reservation-bearing result.
#[test]
fn publish_block_reconstitution_requires_result() {
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
        ReviewPassAcceptedInputEvidence::new(accepted_input_id(5), session_id(4), Some(turn_id(6))),
        state,
        Some(ReviewPassTurnEvidence::new(
            turn_id(6),
            session_id(4),
            accepted_input_id(5),
            ReviewPassTurnOutcome::ReconciliationRequired,
            Some(frontier_id(8)),
        )),
    );
    assert_pass_reconstitution_rejects(input, ReviewPassReconstitutionFailure::IncompatibleResult);
}

/// pass construction requires an accepted input with a canonical
/// origin turn.
#[test]
fn pass_construction_rejects_input_without_origin_turn() {
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

/// pass reconstitution rejects accepted input that no longer
/// authenticates an origin turn.
#[test]
fn pass_reconstitution_rejects_input_without_origin_turn() {
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

/// rejection reason is part of the exact event result.
#[test]
fn finding_history_rejects_mismatched_result_reason() {
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

/// a duplicate's referenced identity is part of the exact event
/// result.
#[test]
fn finding_history_rejects_mismatched_result_reference() {
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

/// a duplicate's authenticated admission status is part of the
/// exact event result.
#[test]
fn finding_history_rejects_mismatched_result_reference_status() {
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

/// judgment cannot accept a finding below the frozen threshold.
#[test]
fn finding_rejects_acceptance_below_policy_threshold() {
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

/// publication cannot post a finding below the frozen threshold.
#[test]
fn finding_rejects_posting_below_policy_threshold() {
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

/// severity-label uncertainty never suppresses a real finding.
#[test]
fn finding_thresholds_ignore_severity_label_confidence() {
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

/// a reservation provider must be the canonical target provider.
#[test]
fn external_link_rejects_provider_mismatch() {
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

/// a posted attachment result must belong to the exact finding
/// association it commits.
#[test]
fn external_link_rejects_posted_result_for_target_association() {
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

/// the same canonical object may follow one moving change request
/// to a refreshed target snapshot.
#[test]
fn external_object_claim_accepts_refreshed_change_request() {
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

/// independently loaded target evidence cannot contradict the
/// provider frozen by the external-link reservation.
#[test]
fn external_object_claim_rejects_cross_wired_target_provider() {
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

/// one canonical object cannot move to an unrelated change
/// request.
#[test]
fn external_object_claim_rejects_unrelated_change_request() {
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

/// immutable commit snapshots never share one canonical external
/// object.
#[test]
fn external_object_claim_rejects_commit_reassociation() {
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

/// duplicate admission preserves each finding's independent
/// producing run and pass when target and frozen policy are equal.
#[test]
fn finding_admits_authenticated_cross_run_reference() {
    let target = target_id(CANONICAL_TARGET_SEED);
    let subject_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
    let canonical_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
    let subject = open_finding_with_producer(subject_reference, ReviewPolicy::version_one());
    let canonical = open_finding_with_producer(canonical_reference, ReviewPolicy::version_one());
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

/// a cross-run reference cannot cross immutable target identity.
#[test]
fn finding_rejects_cross_target_reference() {
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

/// a cross-run reference cannot cross complete frozen policy.
#[test]
fn finding_rejects_cross_policy_reference() {
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

/// complete-graph reconstitution rejects an edge whose frozen
/// policy contradicts the independently supplied referenced root.
#[test]
fn reconstituted_reference_graph_rejects_root_policy_mismatch() {
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

/// pass-result projection independently rejects cross-policy
/// referenced evidence before finding-event admission.
#[test]
fn pass_result_projection_rejects_cross_policy_reference() {
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

/// referenced-finding reconstitution authenticates every element
/// of target/run/pass/finding ancestry against the sealed producer.
#[test]
fn referenced_finding_reconstitution_rejects_cross_wired_ancestry() {
    let target = target_id(CANONICAL_TARGET_SEED);
    let canonical_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
    let canonical = open_finding_with_producer(canonical_reference, ReviewPolicy::version_one());
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

/// referenced-finding reconstitution rejects a producer inventory
/// containing an identity owned by another pass.
#[test]
fn referenced_finding_reconstitution_rejects_unsealed_inventory() {
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

/// a terminal subject cannot acquire a later duplicate edge even
/// to an otherwise eligible same-target same-policy finding.
#[test]
fn terminal_finding_cannot_acquire_reference() {
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
    let canonical = open_finding_with_producer(canonical_reference, ReviewPolicy::version_one());
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

/// complete reconstitution rejects a direct two-finding cycle
/// even when each stored admission status was independently eligible.
#[test]
fn reconstituted_reference_graph_rejects_direct_cycle() {
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

/// complete reconstitution follows independent producer ancestry
/// across runs and rejects a transitive three-finding cycle.
#[test]
fn reconstituted_reference_graph_rejects_transitive_cycle() {
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

/// complete reconstitution rejects an edge whose independently
/// authenticated referenced root is absent from the supplied target graph.
#[test]
fn reconstituted_reference_graph_rejects_missing_root() {
    let target = target_id(CANONICAL_TARGET_SEED);
    let subject_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
    let canonical_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
    let subject_open = open_finding_with_producer(subject_reference, ReviewPolicy::version_one());
    let canonical = open_finding_with_producer(canonical_reference, ReviewPolicy::version_one());
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

/// supersession admits the same authenticated cross-run boundary
/// as duplicate classification without reparenting either finding.
#[test]
fn finding_admits_authenticated_cross_run_supersession() {
    let target = target_id(CANONICAL_TARGET_SEED);
    let subject_reference = finding_ref_for_producer(target, PRODUCING_PASS_SEED);
    let successor_reference = finding_ref_for_producer(target, REASSIGNED_PASS_SEED);
    let subject = open_finding_with_producer(subject_reference, ReviewPolicy::version_one());
    let successor = open_finding_with_producer(successor_reference, ReviewPolicy::version_one());
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

/// referenced-finding reconstitution rejects a producer whose
/// canonical operation is not successful read-only review.
#[test]
fn referenced_finding_reconstitution_rejects_incompatible_producer_kind() {
    let reference = finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
    let inventory =
        ReviewProducedFindings::try_new(vec![reference]).expect("fixture inventory is canonical");
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

/// referenced-finding reconstitution rejects incomplete or
/// contradictory producer terminal evidence.
#[test]
fn referenced_finding_reconstitution_rejects_incomplete_producer_outcome() {
    let reference = finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
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

/// referenced-finding reconstitution rejects a run that
/// contradicts its producer's complete frozen policy.
#[test]
fn referenced_finding_reconstitution_rejects_producer_policy_mismatch() {
    let reference = finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
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

/// a complete graph cannot substitute the same aggregate root
/// twice while claiming complete target coverage.
#[test]
fn reconstituted_reference_graph_rejects_duplicate_root() {
    let reference = finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
    let finding = open_finding_with_producer(reference, ReviewPolicy::version_one());

    let error = validate_complete_review_finding_reference_graph(&[finding.clone(), finding])
        .expect_err("complete graph cannot repeat a finding root");

    assert_eq!(
        *error,
        ReviewFindingReferenceGraphError::DuplicateFinding { reference },
    );
}

/// a complete graph for one immutable target cannot include a
/// disconnected root from another target.
#[test]
fn reconstituted_reference_graph_rejects_foreign_target_root() {
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
    let finding = finding_ref_for_producer(target_id(CANONICAL_TARGET_SEED), PRODUCING_PASS_SEED);
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
