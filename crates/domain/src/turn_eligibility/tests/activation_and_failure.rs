//! Turn scheduling activation and failure tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    ActiveReconstitutionFacts, accepted_origin, activation, current_session, frontier,
    imported_session, matching_active_attempt, queued_input, semantic_entry,
};
use super::*;

/// ancestry-free first eligibility fixes the
/// origin-only frontier and enters Running with one Prepared attempt in
/// the same sealed candidate.
#[test]
fn first_eligibility_prepares_one_atomic_activation_candidate() {
    let session = current_session();
    let queued = accepted_origin(1);
    let activation = activation(1);
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.record(&session, AcceptedInputTurnSchedulingRecordState::Queued)],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    );

    let candidate = input
        .reconstitute()
        .expect("a complete queued projection is valid")
        .prepare_earliest_queued_activation(activation.identities())
        .expect("the sole queued turn is eligible with no active slot");

    assert_eq!(candidate.turn().turn(), queued.turn());
    assert_eq!(
        candidate.turn().accepted_input().id(),
        queued.accepted_input()
    );
    assert_eq!(
        candidate.origin_entry().payload(),
        &InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: queued.accepted_input(),
        }
    );
    assert_eq!(
        candidate.start().lineage(),
        AcceptedInputStartingLineage::FirstInSession
    );
    assert_eq!(
        candidate
            .starting_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![activation.origin_entry().reference(&session)]
    );
    assert!(matches!(
        candidate.turn().phase(),
        ActiveTurnPhase::Running { current_attempt }
            if current_attempt.id() == activation.initial_attempt()
                && current_attempt.state() == &crate::CurrentTurnAttemptState::Prepared
    ));
}

/// an imported session's first native activation
/// appends its origin to the exact checked seed prefix without changing
/// first-in-session lineage.
#[test]
fn first_native_frontier_appends_to_imported_seed() {
    let imported = imported_session();
    let session = imported.session().clone();
    let seed_entries = imported
        .seed_snapshot()
        .ordered_entries()
        .collect::<Vec<_>>();
    let queued = accepted_origin(1);
    let activation = activation(1);

    let candidate = queued_input(&session, queued)
        .with_imported_session(imported)
        .reconstitute()
        .expect("the exact imported seed admits queued native work")
        .prepare_earliest_queued_activation(activation.identities())
        .expect("the first native turn appends to the imported seed");

    let mut expected = seed_entries;
    expected.push(activation.origin_entry().reference(&session));
    assert_eq!(
        candidate
            .starting_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        candidate.start().lineage(),
        AcceptedInputStartingLineage::FirstInSession
    );
}

/// restart returns a queued scheduling projection with no
/// manufactured start, and a cross-wired OriginOf fact fails closed.
#[test]
fn checked_reconstitution_preserves_queued_state_and_exact_origin() {
    let session = current_session();
    let origin = accepted_origin(1);
    let queued = origin.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.clone()],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    )
    .reconstitute()
    .expect("the complete queued record is valid");
    let reconstituted = projection
        .turn(origin.turn())
        .expect("the stored queued turn remains present");
    assert_eq!(
        reconstituted.status(),
        AcceptedInputTurnSchedulingStatus::Queued
    );
    assert_eq!(reconstituted.start(), None);

    let wrong_turn = turn_id(99);
    let cross_wired = AcceptedInputTurnSchedulingRecord::new(
        queued.stored_session(),
        queued.turn(),
        queued.accepted_input_session(),
        AcceptedInputLifecycle::new(
            queued.accepted_input().id(),
            AcceptedInputDisposition::OriginOf(wrong_turn),
        ),
        queued.queue_session(),
        queued.queue_turn(),
        queued.order(),
        queued.origin_delivery(),
        queued.origin_configuration().clone(),
        queued.state().clone(),
    );
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let error = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        vec![cross_wired],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    )
    .reconstitute()
    .expect_err("the exact OriginOf(turn) correlation is required");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptedInputOriginMismatch {
            turn: origin.turn(),
        }
    );
}

/// an admitted active restart record owns its exact
/// Prepared attempt, reconstructs Running, and makes that identity
/// unavailable to a second activation candidate.
#[test]
fn active_reconstitution_requires_and_exposes_exact_prepared_attempt() {
    let session = current_session();
    let active_origin = accepted_origin(1);
    let stored_attempt = matching_active_attempt();
    let facts = ActiveReconstitutionFacts::matching(&session, active_origin);
    let projection = facts
        .input()
        .reconstitute()
        .expect("the active turn has its exact prepared attempt");
    let active = projection
        .active_turn()
        .expect("the reconstructed turn owns the active slot");
    assert!(matches!(
        active.active_phase(),
        Some(ActiveTurnPhase::Running { current_attempt })
            if current_attempt.id() == stored_attempt
                && current_attempt.state() == &CurrentTurnAttemptState::Prepared
    ));

    let colliding_activation = activation(1);
    let collision = projection
        .clone()
        .prepare_earliest_queued_activation(
            colliding_activation.identities_with_attempt(stored_attempt),
        )
        .expect_err("a current attempt identity cannot be proposed again");
    assert_eq!(
        collision.failure(),
        AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists
    );
    let occupied_activation = activation(2);
    let occupied = projection
        .prepare_earliest_queued_activation(occupied_activation.identities())
        .expect_err("an active slot blocks every queued activation");
    assert_eq!(
        occupied.failure(),
        AcceptedInputEligibilityFailure::ActiveTurnPresent {
            turn: active_origin.turn(),
        }
    );
}

/// inert prepared facts become a canonical attempt only
/// inside the validated owner projection.
#[test]
fn active_reconstitution_derives_prepared_attempt_after_validation() {
    let session = current_session();
    let active = accepted_origin(1);
    let expected_attempt = matching_active_attempt();
    let facts = ActiveReconstitutionFacts::matching(&session, active);
    let projection = facts
        .input()
        .reconstitute()
        .expect("the complete owner projection derives the prepared attempt");
    let phase = projection
        .active_turn()
        .expect("the turn owns the active slot")
        .active_phase();
    assert!(matches!(
        phase,
        Some(ActiveTurnPhase::Running { current_attempt })
            if current_attempt.id() == expected_attempt
                && current_attempt.state() == &CurrentTurnAttemptState::Prepared
    ));
}

/// inert running facts traverse the sealed
/// prepared-to-running transition only inside the validated owner
/// projection.
#[test]
fn active_reconstitution_derives_running_attempt_after_validation() {
    let session = current_session();
    let active = accepted_origin(1);
    let expected_attempt = turn_attempt_id(51);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::running(
        active.turn(),
        expected_attempt,
    ));
    let projection = facts
        .input()
        .reconstitute()
        .expect("the complete owner projection derives the running attempt");
    let execution = projection
        .active_turn_execution()
        .expect("active scheduling facts seal execution ownership");
    assert_eq!(execution.turn(), active.turn());
    assert!(matches!(
        execution.phase(),
        ActiveTurnPhase::Running { current_attempt }
            if current_attempt.id() == expected_attempt
                && current_attempt.state() == &CurrentTurnAttemptState::Running
    ));
}

/// a running continuation retains the exact
/// independently checked tool batch correlation needed by interruption.
#[test]
fn running_tool_batch_correlation_is_reconstituted() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
    let starting_frontier = ActiveReconstitutionFacts::matching_starting_frontier();
    let producing_call = model_call_id(50);
    let continuation_attempt = turn_attempt_id(60);
    let request_id = tool_request_id(70);
    let assistant_tool_entry = semantic_entry(31);
    let yielded_frontier = frontier(41);
    let request = ToolRequestReconstitutionInput::new(
        request_id,
        session.id(),
        active.turn(),
        producing_call,
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("current_time")).expect("fixture name is canonical"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are canonical"),
    )
    .into_request();
    let approval = ToolApprovalResolutionReconstitutionInput::user_fixture(
        request_id,
        ToolApprovalDecision::Approve,
    )
    .reconstitute()
    .expect("user approval is implemented");
    let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        yielded_frontier.id(),
        vec![
            origin_entry.reference(&session),
            assistant_tool_entry.reference(&session),
        ],
    )
    .expect("the tool response extends the starting frontier");
    let batch = ToolBatchReconstitutionInput::new(
        session.id(),
        active.turn(),
        producing_call,
        yielded,
        vec![request],
        vec![approval],
        vec![],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: continuation_attempt,
        },
    )
    .reconstitute()
    .expect("the complete approved batch is executing");
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.replace_active_phase(
        ActiveTurnSchedulingReconstitutionInput::prepared(active.turn(), continuation_attempt)
            .with_executing_tool_batch(&batch),
    );
    facts
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            assistant_tool_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request: request_id,
            },
        ));
    facts
        .snapshots
        .push(yielded_frontier.snapshot(&session, &[origin_entry, assistant_tool_entry]));
    let model_call = ModelCallReconstitutionInput::new(
        producing_call,
        active.turn(),
        turn_attempt_id(59),
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        starting_frontier.id(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
    );

    let projection = facts
        .input()
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                active.turn(),
                model_call.target(),
            )],
            vec![model_call],
        )
        .reconstitute()
        .expect("the running batch is bound to its exact call and yielded frontier");

    assert_eq!(
        projection.active_executing_tool_batch,
        Some(ActiveExecutingToolBatchCorrelation {
            session: session.id(),
            turn: active.turn(),
            producing_call,
            yielded_frontier: yielded_frontier.id(),
            turn_attempt: Some(continuation_attempt),
        })
    );
    let active_execution = projection
        .active_turn_execution()
        .expect("the correlated continuation owns the active slot");
    let ActiveTurnPhase::Running { current_attempt } = active_execution.phase() else {
        panic!("the correlated continuation is the running phase");
    };
    assert_eq!(current_attempt.id(), continuation_attempt);
    assert_eq!(current_attempt.state(), &CurrentTurnAttemptState::Prepared);
}

/// startup recovery consumes the complete active
/// projection, ends its exact evidence-free attempt as Lost, and appends
/// one `TurnFailed` marker to the starting frontier.
#[test]
fn prepares_atomic_lost_failed_terminal_candidate() {
    let session = current_session();
    let active = accepted_origin(1);
    let failure_entry = semantic_entry(500);
    let terminal_frontier = frontier(600);
    let identities =
        AcceptedInputTurnFailureIdentities::new(failure_entry.id(), terminal_frontier.id());
    let projection = ActiveReconstitutionFacts::matching(&session, active)
        .input()
        .reconstitute()
        .expect("the complete active projection is valid");

    let candidate = projection
        .prepare_active_turn_lost_failure(identities)
        .expect("evidence-free prior-process work can end Lost");

    assert_eq!(candidate.turn().turn(), active.turn());
    assert_eq!(
        candidate.turn().ended_attempt().id(),
        matching_active_attempt()
    );
    assert_eq!(
        candidate.turn().ended_attempt().end(),
        &AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Lost,
        }
    );
    assert_eq!(candidate.turn().disposition(), &TurnDisposition::Failed);
    assert_eq!(
        candidate.failure_entry().payload(),
        &InitialSemanticTranscriptEntryPayload::TurnFailed {
            turn: active.turn(),
        }
    );
    assert_eq!(
        candidate
            .terminal_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            ActiveReconstitutionFacts::matching_origin_entry().reference(&session),
            failure_entry.reference(&session),
        ]
    );
    assert_eq!(
        candidate.terminal_snapshot().frontier().snapshot(),
        terminal_frontier.id()
    );
}

/// the same Lost failure transition is valid for a stored
/// Running attempt, without inventing a stop cause.
#[test]
fn running_attempt_also_prepares_without_stop_lost() {
    let session = current_session();
    let active = accepted_origin(1);
    let running_attempt = turn_attempt_id(51);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::running(
        active.turn(),
        running_attempt,
    ));

    let candidate = facts
        .input()
        .reconstitute()
        .expect("the complete running projection is valid")
        .prepare_active_turn_lost_failure(AcceptedInputTurnFailureIdentities::new(
            semantic_entry(500).id(),
            frontier(600).id(),
        ))
        .expect("running prior-process work can end Lost");

    assert_eq!(candidate.turn().ended_attempt().id(), running_attempt);
    assert_eq!(
        candidate.turn().ended_attempt().end(),
        &AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Lost,
        }
    );
}

/// pending steering is not a stop cause; the lost
/// failure reclassifies it into a queued successor, and identities that do
/// not match the pending inventory leave the projection unchanged.
#[test]
fn lost_failure_reclassifies_pending_steering() {
    let session = current_session();
    let active = accepted_origin(1);
    let pending = accepted_origin(2);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    let tail = facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts contain the active tail");
    tail.observed_last_position = pending.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            pending.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    let projection = facts
        .input()
        .reconstitute()
        .expect("the pending-steering tail is complete");
    let identities =
        AcceptedInputTurnFailureIdentities::new(semantic_entry(500).id(), frontier(600).id());

    let error = projection
        .clone()
        .prepare_active_turn_lost_failure(identities.clone())
        .expect_err("pending steering needs a successor identity");
    assert_eq!(error.projection(), &projection);
    assert_eq!(error.identities(), &identities);
    assert_eq!(
        error.failure(),
        AcceptedInputTurnFailureFailure::PendingSteeringReclassificationMismatch
    );

    let successor = turn_id(700);
    let candidate = projection
        .prepare_active_turn_lost_failure(identities.with_pending_steering_reclassifications(vec![
            PendingSteeringReclassificationIdentity::new(pending.accepted_input(), successor),
        ]))
        .expect("pending steering is reclassified rather than refused");
    let [reclassified] = candidate.reclassified_pending_steering() else {
        panic!("exactly one successor is reclassified");
    };
    assert_eq!(reclassified.turn(), successor);
    assert_eq!(reclassified.source_turn(), active.turn());
    assert_eq!(
        reclassified.accepted_input(),
        &AcceptedInputLifecycle::new(
            pending.accepted_input(),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: successor,
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        )
    );
    assert_eq!(
        reclassified.order(),
        AcceptedInputQueueOrder::ordinary(pending.position())
    );
}

/// pending steering remains outside the active rendered frontier
/// until a safe-point continuation incorporates it.
#[test]
fn pending_steering_is_not_an_active_rendered_frontier_origin() {
    let session = current_session();
    let active = accepted_origin(1);
    let pending = accepted_origin(2);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    let tail = facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts contain the active tail");
    tail.observed_last_position = pending.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            pending.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    let projection = facts
        .input()
        .reconstitute()
        .expect("the pending-steering tail is complete");

    assert_eq!(
        projection.active_rendered_frontier_origins(),
        Some(vec![active.accepted_input()])
    );
}

/// startup failure preparation rejects each committed
/// identity before constructing a candidate.
#[test]
fn rejects_committed_failure_identities() {
    let session = current_session();
    let active = accepted_origin(1);
    let projection = ActiveReconstitutionFacts::matching(&session, active)
        .input()
        .reconstitute()
        .expect("the complete active projection is valid");

    let entry_collision = projection
        .clone()
        .prepare_active_turn_lost_failure(AcceptedInputTurnFailureIdentities::new(
            ActiveReconstitutionFacts::matching_origin_entry().id(),
            frontier(600).id(),
        ))
        .expect_err("the semantic identity is already committed");
    assert_eq!(
        entry_collision.failure(),
        AcceptedInputTurnFailureFailure::FailureEntryIdentityAlreadyExists
    );

    let frontier_collision = projection
        .prepare_active_turn_lost_failure(AcceptedInputTurnFailureIdentities::new(
            semantic_entry(500).id(),
            ActiveReconstitutionFacts::matching_starting_frontier().id(),
        ))
        .expect_err("the frontier identity is already committed");
    assert_eq!(
        frontier_collision.failure(),
        AcceptedInputTurnFailureFailure::TerminalFrontierIdentityAlreadyExists
    );
}
