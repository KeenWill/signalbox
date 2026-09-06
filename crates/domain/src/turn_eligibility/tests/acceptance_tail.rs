//! Turn scheduling acceptance tail tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    ActiveReconstitutionFacts, OriginRecordFacts, ReconstitutionFailureRow, accepted_origin,
    active_input_after_historical_interrupt, assert_input_rejects_unchanged,
    assert_reconstitution_rejects_unchanged, current_session, default_origin_delivery, frontier,
};
use super::*;

/// an active scheduling projection
/// requires the exact session-scoped interval anchored at its origin; a
/// missing, cross-session, or cross-wired interval fails closed.
#[test]
fn active_reconstitution_requires_exact_session_acceptance_tail_identity() {
    let session = current_session();
    let active = accepted_origin(1);

    let missing = assert_reconstitution_rejects_unchanged(ActiveReconstitutionFacts {
        acceptance_tail: None,
        ..ActiveReconstitutionFacts::matching(&session, active)
    });
    assert_eq!(
        missing,
        AcceptedInputSchedulingReconstitutionFailure::MissingActiveAcceptanceTail {
            turn: active.turn(),
        }
    );

    let other_session = session_id(2);
    let mut wrong_session_facts = ActiveReconstitutionFacts::matching(&session, active);
    wrong_session_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail")
        .session = other_session;
    let wrong_session = assert_reconstitution_rejects_unchanged(wrong_session_facts);
    assert_eq!(
        wrong_session,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailSessionMismatch {
            expected: session.id(),
            actual: other_session,
        }
    );

    let other_anchor = accepted_input_id(99);
    let mut wrong_anchor_facts = ActiveReconstitutionFacts::matching(&session, active);
    wrong_anchor_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail")
        .anchor = other_anchor;
    let wrong_anchor = assert_reconstitution_rejects_unchanged(wrong_anchor_facts);
    assert_eq!(
        wrong_anchor,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
            turn: active.turn(),
            expected: active.accepted_input(),
            actual: other_anchor,
        }
    );

    expect![[r#"
            ┌──────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact    │ failure                                                                                                                                                                                                             │
            ├──────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ active tail omitted      │ MissingActiveAcceptanceTail { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }                                                                                                                                  │
            │ tail session cross-wired │ AcceptanceTailSessionMismatch { expected: SessionId(00000000-0000-0000-0000-000000000001), actual: SessionId(00000000-0000-0000-0000-000000000002) }                                                                │
            │ tail anchor cross-wired  │ AcceptanceTailAnchorMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe), expected: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffe), actual: AcceptedInputId(00000000-0000-0000-0000-000000000063) } │
            └──────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "active tail omitted",
                failure: format!("{missing:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail session cross-wired",
                failure: format!("{wrong_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail anchor cross-wired",
                failure: format!("{wrong_anchor:?}"),
            },
        ]));
}

/// every position from the active origin through
/// the observed session tail is present exactly once and every
/// pending-steering disposition remains bound to that active turn.
#[test]
fn active_reconstitution_rejects_gapped_or_misbound_acceptance_tail() {
    let session = current_session();
    let active = accepted_origin(1);
    let second = accepted_origin(2);
    let third = accepted_origin(3);

    let mut gapped_facts = ActiveReconstitutionFacts::matching(&session, active);
    let gapped_tail = gapped_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail");
    gapped_tail.observed_last_position = third.position();
    gapped_tail
        .entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                second.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            third.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    let gapped = assert_reconstitution_rejects_unchanged(gapped_facts);
    assert_eq!(
        gapped,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailPositionMismatch {
            accepted_input: second.accepted_input(),
            expected: second.position(),
            actual: third.position(),
        }
    );

    let other_turn = turn_id(99);
    let mut misbound_facts = ActiveReconstitutionFacts::matching(&session, active);
    let misbound_tail = misbound_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail");
    misbound_tail.observed_last_position = second.position();
    misbound_tail
        .entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                second.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(other_turn),
                },
            ),
            second.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: other_turn,
            },
        ));
    let misbound = assert_reconstitution_rejects_unchanged(misbound_facts);
    assert_eq!(
        misbound,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: second.accepted_input(),
        }
    );

    let after_active_delivery = DeliveryRequest::AfterCurrentTurn {
        expected_active_turn: active.turn(),
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let mut cross_wired_facts = ActiveReconstitutionFacts::matching(&session, active);
    let cross_wired_tail = cross_wired_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail");
    cross_wired_tail.observed_last_position = third.position();
    cross_wired_tail
        .entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                second.accepted_input(),
                AcceptedInputDisposition::OriginOf(second.turn()),
            ),
            second.position(),
            after_active_delivery,
        ));
    cross_wired_tail
        .entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                third.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            third.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    cross_wired_facts.turns.push(second.record_with(
        &session,
        OriginRecordFacts {
            order: AcceptedInputQueueOrder::ordinary(third.position()),
            delivery: after_active_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    ));
    let cross_wired = assert_reconstitution_rejects_unchanged(cross_wired_facts);
    assert_eq!(
        cross_wired,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: second.accepted_input(),
        }
    );

    expect![[r#"
            ┌────────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact              │ failure                                                                                                                                                                      │
            ├────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ interior position omitted          │ AcceptanceTailPositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd), expected: SessionInputPosition(2), actual: SessionInputPosition(3) } │
            │ pending steering owner cross-wired │ AcceptanceTailDispositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd) }                                                                  │
            │ origin position cross-wired        │ AcceptanceTailDispositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd) }                                                                  │
            └────────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "interior position omitted",
                failure: format!("{gapped:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "pending steering owner cross-wired",
                failure: format!("{misbound:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "origin position cross-wired",
                failure: format!("{cross_wired:?}"),
            },
        ]));
}

/// a newly active queued origin retains later acceptance
/// positions already consumed by its terminal predecessor, while only its
/// own consumed steering reaches the active execution aggregate.
#[test]
fn active_tail_retains_predecessor_consumed_steering() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let active = accepted_origin(2);
    let predecessor_consumed = accepted_origin(3);
    let active_consumed = accepted_origin(4);
    let predecessor_record = predecessor.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: frontier(70).id(),
            terminal_execution: None,
            terminal_frontier: frontier(71).id(),
        },
    );
    let active_record = active.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor.turn(),
            },
            starting_frontier: frontier(72).id(),
            phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                active.turn(),
                turn_attempt_id(73),
            ),
        },
    );
    let records = BTreeMap::from([
        (predecessor.turn(), &predecessor_record),
        (active.turn(), &active_record),
    ]);
    let accepted_input_turns = BTreeMap::from([
        (predecessor.accepted_input(), predecessor.turn()),
        (active.accepted_input(), active.turn()),
    ]);
    let execution_position_by_turn = BTreeMap::from([(predecessor.turn(), 0), (active.turn(), 1)]);
    let tail_input = SessionAcceptanceTailReconstitutionInput::new(
        session.id(),
        active.accepted_input(),
        active_consumed.position(),
        vec![
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    active.accepted_input(),
                    AcceptedInputDisposition::OriginOf(active.turn()),
                ),
                active.position(),
                default_origin_delivery(),
            ),
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    predecessor_consumed.accepted_input(),
                    AcceptedInputDisposition::ConsumedAsSteering {
                        call: model_call_id(74),
                    },
                ),
                predecessor_consumed.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: predecessor.turn(),
                },
            ),
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    active_consumed.accepted_input(),
                    AcceptedInputDisposition::ConsumedAsSteering {
                        call: model_call_id(75),
                    },
                ),
                active_consumed.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ),
        ],
    );
    let tail = reconstitute_active_acceptance_tail(
        session.id(),
        Some(active.turn()),
        Some(&tail_input),
        ActiveAcceptanceTailReconstitutionEvidence {
            records_by_turn: &records,
            accepted_input_turns: &accepted_input_turns,
            consumed_inputs: &BTreeMap::from([
                (predecessor_consumed.accepted_input(), model_call_id(74)),
                (active_consumed.accepted_input(), model_call_id(75)),
            ]),
            preceding_non_accepted_terminals: &BTreeSet::new(),
            execution_position_by_turn: &execution_position_by_turn,
        },
    )
    .expect("the terminal predecessor's consumed steering remains valid history")
    .expect("an active turn retains its complete acceptance tail");
    let (pending, consumed) = active_execution_steering_inputs(active.turn(), &tail);

    assert!(pending.is_empty());
    assert_eq!(consumed.len(), 1);
    assert_eq!(
        consumed[0].accepted_input(),
        active_consumed.accepted_input()
    );
    assert_eq!(consumed[0].source_turn(), active.turn());

    let mut cross_wired_tail = tail_input.clone();
    cross_wired_tail.entries[1] = SessionAcceptanceTailEntryReconstitutionInput::new(
        session.id(),
        AcceptedInputLifecycle::new(
            predecessor_consumed.accepted_input(),
            AcceptedInputDisposition::ConsumedAsSteering {
                call: model_call_id(76),
            },
        ),
        predecessor_consumed.position(),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: predecessor.turn(),
        },
    );
    let failure = reconstitute_active_acceptance_tail(
        session.id(),
        Some(active.turn()),
        Some(&cross_wired_tail),
        ActiveAcceptanceTailReconstitutionEvidence {
            records_by_turn: &records,
            accepted_input_turns: &accepted_input_turns,
            consumed_inputs: &BTreeMap::from([
                (predecessor_consumed.accepted_input(), model_call_id(74)),
                (active_consumed.accepted_input(), model_call_id(75)),
            ]),
            preceding_non_accepted_terminals: &BTreeSet::new(),
            execution_position_by_turn: &execution_position_by_turn,
        },
    )
    .expect_err("cross-wired historical steering must fail closed");

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: predecessor_consumed.accepted_input(),
        }
    );
}

/// later-accepted interrupt work executes before the
/// ordinary origin it displaced, so steering consumed by that interrupt
/// remains historical rather than becoming active execution input.
#[test]
fn active_tail_rejects_unproven_historical_consumed_steering() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let active = accepted_origin(2);
    let interrupt_successor = accepted_origin(3);
    let interrupt_consumed = accepted_origin(4);
    let mut input =
        active_input_after_historical_interrupt(&session, predecessor, active, interrupt_successor);
    let tail = input
        .active_acceptance_tail
        .as_mut()
        .expect("the historical-interrupt helper supplies an active tail");
    tail.observed_last_position = interrupt_consumed.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                interrupt_consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: model_call_id(76),
                },
            ),
            interrupt_consumed.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: interrupt_successor.turn(),
            },
        ));
    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: interrupt_consumed.accepted_input(),
        }
    );
}
