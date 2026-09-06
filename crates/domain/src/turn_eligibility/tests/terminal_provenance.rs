//! Turn scheduling terminal provenance tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    FailedTerminalReconstitutionFacts, OriginFixture, OriginRecordFacts, accepted_origin,
    assert_input_rejects_unchanged, current_session, frontier, semantic_entry,
};
use super::*;

#[track_caller]
fn assert_failed_terminal_call_provenance_is_complete(
    session: &Session,
    failed: OriginFixture,
    attempt: TurnAttemptId,
    call_disposition: ModelCallDisposition,
) {
    let origin_entry = FailedTerminalReconstitutionFacts::matching_origin_entry();
    let failure_entry = FailedTerminalReconstitutionFacts::matching_failure_entry();
    let steering_entry = semantic_entry(32);
    let consumed = accepted_origin(2);
    let call_frontier = frontier(42);
    let terminal_frontier = FailedTerminalReconstitutionFacts::matching_terminal_frontier();
    let call_id = model_call_id(50);
    let mut facts = FailedTerminalReconstitutionFacts::matching(session, failed);
    facts
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            steering_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: consumed.accepted_input(),
                source_turn: failed.turn(),
            },
        ));
    facts
        .snapshots
        .retain(|snapshot| snapshot.snapshot() != terminal_frontier.id());
    facts.snapshots.extend([
        call_frontier.snapshot(session, &[origin_entry, steering_entry]),
        terminal_frontier.snapshot(session, &[origin_entry, steering_entry, failure_entry]),
    ]);
    facts.replace_terminal_execution(Some(FailedTurnExecutionReconstitutionInput::with_call(
        failed.turn(),
        attempt,
        UnstoppedAttemptDisposition::KnownFailure,
        call_id,
    )));
    let call = ModelCallReconstitutionInput::new(
        call_id,
        failed.turn(),
        attempt,
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        call_frontier.id(),
        ModelCallReconstitutionState::Terminal(call_disposition),
    );
    let projection = facts
        .input()
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                failed.turn(),
                call.target(),
            )],
            vec![call],
        )
        .with_consumed_steering_facts(vec![ConsumedSteeringReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering { call: call_id },
            ),
            consumed.position(),
            failed.turn(),
        )])
        .reconstitute()
        .expect("failed terminal call provenance is fully correlated");
    assert_eq!(
        projection.attempt_owners.get(&attempt),
        Some(&failed.turn())
    );
    assert!(
        projection
            .semantic_entries
            .contains_key(&origin_entry.reference(session))
    );
}

/// S02 / S03: failed-terminal reconstitution
/// preserves all three accepted execution shapes and any steering already
/// committed in an ended call's source frontier.
#[test]
fn s02_s03_failed_terminal_execution_provenance_is_complete() {
    let session = current_session();
    let failed = accepted_origin(1);
    let attempt = turn_attempt_id(60);

    let direct_failure = FailedTerminalReconstitutionFacts::matching(&session, failed)
        .input()
        .reconstitute()
        .expect("a direct static failure has no execution provenance");
    assert!(direct_failure.attempt_owners.is_empty());

    let mut attempt_only_facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    attempt_only_facts.replace_terminal_execution(Some(
        FailedTurnExecutionReconstitutionInput::attempt_only(
            failed.turn(),
            attempt,
            UnstoppedAttemptDisposition::Lost,
        ),
    ));
    let attempt_only = attempt_only_facts
        .input()
        .reconstitute()
        .expect("startup loss retains its exact ended attempt");
    assert_eq!(
        attempt_only.attempt_owners.get(&attempt),
        Some(&failed.turn())
    );

    assert_failed_terminal_call_provenance_is_complete(
        &session,
        failed,
        attempt,
        ModelCallDisposition::KnownFailed,
    );
    assert_failed_terminal_call_provenance_is_complete(
        &session,
        failed,
        attempt,
        ModelCallDisposition::Cancelled,
    );
}

/// S02 / S07: a proof-bearing known-failure attempt
/// can only correlate a physically known-failed call. Confirmed physical
/// cancellation remains the cancelled terminal outcome.
#[test]
fn s02_s07_stopped_failure_rejects_cancelled_call() {
    let session = current_session();
    let failed = accepted_origin(1);
    let successor = accepted_origin(2);
    let attempt = turn_attempt_id(60);
    let call_id = model_call_id(50);
    let starting_frontier = FailedTerminalReconstitutionFacts::matching_starting_frontier();
    let successor_order =
        AcceptedInputQueueOrder::interrupt_immediately_after(successor.position(), failed.turn());
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        failed.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    facts.replace_terminal_execution(Some(
        FailedTurnExecutionReconstitutionInput::with_call_after_cancellation(
            failed.turn(),
            attempt,
            CancellationStopDisposition::KnownFailure,
            interrupt,
            call_id,
        ),
    ));
    facts.turns.push(successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: failed.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    ));
    let input_for = |disposition| {
        let call = ModelCallReconstitutionInput::new(
            call_id,
            failed.turn(),
            attempt,
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(disposition),
        );
        facts.clone().input().with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                failed.turn(),
                call.target(),
            )],
            vec![call],
        )
    };

    input_for(ModelCallDisposition::KnownFailed)
        .reconstitute()
        .expect("stopped known failure retains its known-failed call");
    assert_eq!(
        assert_input_rejects_unchanged(input_for(ModelCallDisposition::Cancelled)),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: failed.turn(),
        }
    );
}

/// S02 / S03: failed-terminal attempt provenance fails closed
/// when either ownership or the allowed terminal end is contradicted.
#[test]
fn s02_s03_failed_terminal_attempt_provenance_fails_closed() {
    let session = current_session();
    let failed = accepted_origin(1);
    let attempt = turn_attempt_id(60);

    let mut wrong_owner = FailedTerminalReconstitutionFacts::matching(&session, failed);
    wrong_owner.replace_terminal_execution(Some(
        FailedTurnExecutionReconstitutionInput::attempt_only(
            turn_id(99),
            attempt,
            UnstoppedAttemptDisposition::KnownFailure,
        ),
    ));
    assert_eq!(
        assert_input_rejects_unchanged(wrong_owner.input()),
        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptOwnershipMismatch {
            turn: failed.turn(),
            attempt,
        }
    );

    let mut wrong_end = FailedTerminalReconstitutionFacts::matching(&session, failed);
    wrong_end.replace_terminal_execution(Some(
        FailedTurnExecutionReconstitutionInput::attempt_only(
            failed.turn(),
            attempt,
            UnstoppedAttemptDisposition::TurnCompleted,
        ),
    ));
    assert_eq!(
        assert_input_rejects_unchanged(wrong_end.input()),
        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
            turn: failed.turn(),
            attempt,
        }
    );

    let successor = accepted_origin(2);
    let successor_order =
        AcceptedInputQueueOrder::interrupt_immediately_after(successor.position(), failed.turn());
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        failed.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let mut lost_after_cancellation = FailedTerminalReconstitutionFacts::matching(&session, failed);
    lost_after_cancellation.replace_terminal_execution(Some(
        FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
            failed.turn(),
            attempt,
            CancellationStopDisposition::Lost,
            interrupt,
        ),
    ));
    lost_after_cancellation.turns.push(successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: failed.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    ));
    assert_eq!(
        assert_input_rejects_unchanged(lost_after_cancellation.input()),
        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
            turn: failed.turn(),
            attempt,
        }
    );
}

/// S02: a failed terminal call must match the ended
/// attempt and the turn's selection, target, starting frontier, and
/// KnownFailed-or-Cancelled physical disposition.
#[test]
fn s02_failed_terminal_call_provenance_fails_closed() {
    let session = current_session();
    let failed = accepted_origin(1);
    let attempt = turn_attempt_id(60);
    let call_id = model_call_id(50);
    let starting_frontier = FailedTerminalReconstitutionFacts::matching_starting_frontier();
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    facts.replace_terminal_execution(Some(FailedTurnExecutionReconstitutionInput::with_call(
        failed.turn(),
        attempt,
        UnstoppedAttemptDisposition::KnownFailure,
        call_id,
    )));
    let mismatched_call = ModelCallReconstitutionInput::new(
        call_id,
        failed.turn(),
        turn_attempt_id(61),
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        starting_frontier.id(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::KnownFailed),
    );
    let input = facts.input().with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            failed.turn(),
            mismatched_call.target(),
        )],
        vec![mismatched_call],
    );
    assert_eq!(
        assert_input_rejects_unchanged(input),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: failed.turn(),
        }
    );
}
