//! Turn scheduling steering tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    ActiveReconstitutionFacts, ConsumedSteeringReconstitutionFacts, accepted_origin,
    assert_input_rejects_unchanged, configuration, current_session, ended_tool_attempt,
    ended_tool_attempt_with_end, frontier, matching_active_attempt, semantic_entry,
};
use super::*;

/// scheduling
/// reconstitution admits consumed steering only when its semantic subject,
/// accepted lifecycle, source turn, call frontier, and acceptance order
/// agree exactly.
#[test]
fn reconstitution_validates_steering_subjects() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed)
        .input()
        .reconstitute()
        .expect("matching consumed steering reconstructs");

    let mut nonfollowing_position =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    nonfollowing_position.consumed_steering[0].acceptance_position = active.position();
    nonfollowing_position.acceptance_tail.entries[1].position = active.position();
    nonfollowing_position.acceptance_tail.observed_last_position = active.position();
    assert_eq!(
        assert_input_rejects_unchanged(nonfollowing_position.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );

    let mut skipped_reclassified =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    skipped_reclassified.turns[0].state = AcceptedInputTurnSchedulingRecordState::TerminalFailed {
        starting_lineage: AcceptedInputStartingLineage::FirstInSession,
        starting_frontier: ActiveReconstitutionFacts::matching_starting_frontier().id(),
        terminal_execution: None,
        terminal_frontier: frontier(42).id(),
    };
    let reclassified = accepted_origin(2);
    skipped_reclassified
        .turns
        .push(AcceptedInputTurnSchedulingRecord::reclassified(
            session.id(),
            reclassified.turn(),
            session.id(),
            AcceptedInputLifecycle::new(
                reclassified.accepted_input(),
                AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                    turn: reclassified.turn(),
                    reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
                },
            ),
            session.id(),
            reclassified.turn(),
            reclassified.ordinary_order(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
            crate::SteeringBinding::new(active.turn()),
            configuration(&session),
            AcceptedInputTurnSchedulingRecordState::Queued,
        ));
    assert_eq!(
        assert_input_rejects_unchanged(skipped_reclassified.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );

    let mut nonexistent = ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    nonexistent.semantic_entries[1] = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_entry(31).id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
            accepted_input: accepted_input_id(99),
            source_turn: active.turn(),
        },
    );
    assert_eq!(
        assert_input_rejects_unchanged(nonexistent.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );

    let mut wrong_source =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    wrong_source.semantic_entries[1] = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_entry(31).id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
            accepted_input: consumed.accepted_input(),
            source_turn: turn_id(99),
        },
    );
    assert_eq!(
        assert_input_rejects_unchanged(wrong_source.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );

    let mut missing_lifecycle =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    missing_lifecycle.consumed_steering.clear();
    assert_eq!(
        assert_input_rejects_unchanged(missing_lifecycle.input()),
        AcceptedInputSchedulingReconstitutionFailure::SteeringSemanticEntryMismatch {
            entry: semantic_entry(31).id(),
        }
    );

    let mut duplicate_subject =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    duplicate_subject
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            semantic_entry(32).id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: consumed.accepted_input(),
                source_turn: active.turn(),
            },
        ));
    assert_eq!(
        assert_input_rejects_unchanged(duplicate_subject.input()),
        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
            entry: semantic_entry(32).id(),
        }
    );

    let mut duplicate_lifecycle =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    let duplicate = duplicate_lifecycle
        .consumed_steering
        .first()
        .cloned()
        .expect("the matching fixture contains one consumed subject");
    duplicate_lifecycle.consumed_steering.push(duplicate);
    assert_eq!(
        assert_input_rejects_unchanged(duplicate_lifecycle.input()),
        AcceptedInputSchedulingReconstitutionFailure::DuplicateConsumedSteering {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// scheduling reconstitution admits
/// the durable shape the continuation transaction commits — a running
/// continuation attempt owning a prepared steering-consuming call whose
/// frontier is the round's exact result projection plus the consumed
/// suffix.
#[test]
fn steering_consumed_at_continuation_reconstitutes() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed)
        .input()
        .reconstitute()
        .expect("continuation-consumed steering reconstructs");
}

/// a running attempt owning a prepared
/// steering-consuming call is legal only with the round's result
/// evidence.
#[test]
fn continuation_pair_requires_round_evidence() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut missing_evidence =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    missing_evidence.steering_continuation_rounds.clear();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// continuation-round evidence must name a
/// steering-consuming call.
#[test]
fn round_evidence_requires_a_consuming_call() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut dangling_evidence =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    dangling_evidence.steering_continuation_rounds.push(
        SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_producing_call(),
            vec![
                ConsumedSteeringReconstitutionFacts::matching_continuation_round_attempt(
                    &session, active,
                ),
            ],
            Vec::new(),
        ),
    );
    assert_eq!(
        assert_input_rejects_unchanged(dangling_evidence.input()),
        AcceptedInputSchedulingReconstitutionFailure::SteeringContinuationRoundMismatch {
            call: ConsumedSteeringReconstitutionFacts::matching_continuation_producing_call(),
        }
    );
}

/// continuation-round evidence names each
/// consuming call at most once.
#[test]
fn round_evidence_names_each_consumer_once() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut duplicate_evidence =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    let duplicated = duplicate_evidence.steering_continuation_rounds[0].clone();
    duplicate_evidence
        .steering_continuation_rounds
        .push(duplicated);
    assert_eq!(
        assert_input_rejects_unchanged(duplicate_evidence.input()),
        AcceptedInputSchedulingReconstitutionFailure::SteeringContinuationRoundMismatch {
            call: ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
        }
    );
}

/// the consumed steering entries must be
/// the exact trailing suffix after the round's result window.
#[test]
fn consumed_steering_is_the_continuation_trailing_suffix() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut interposed_steering =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    interposed_steering.snapshots[1] = frontier(41).snapshot(
        &session,
        &[
            ActiveReconstitutionFacts::matching_origin_entry(),
            semantic_entry(34),
            semantic_entry(31),
            semantic_entry(35),
        ],
    );
    assert_eq!(
        assert_input_rejects_unchanged(interposed_steering.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// each result entry in the
/// continuation window must correlate to its proposal-ordered request.
#[test]
fn continuation_results_correlate_to_proposal_order() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut miscorrelated_result =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    miscorrelated_result.steering_continuation_rounds =
        vec![SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            vec![ended_tool_attempt(
                &session,
                active,
                matching_active_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                tool_request_id(96),
            )],
            Vec::new(),
        )];
    assert_eq!(
        assert_input_rejects_unchanged(miscorrelated_result.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// the round's tools were issued by
/// the same continuation attempt that owns the consuming call; evidence
/// issued by a foreign attempt fails closed.
#[test]
fn continuation_results_bind_to_the_consuming_attempt() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut foreign_issuing_attempt =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    foreign_issuing_attempt.steering_continuation_rounds =
        vec![SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            vec![ended_tool_attempt(
                &session,
                active,
                turn_attempt_id(49),
                ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
            )],
            Vec::new(),
        )];
    assert_eq!(
        assert_input_rejects_unchanged(foreign_issuing_attempt.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// a continuation window forbids
/// turn-end closures, which exist only in terminal materialization.
#[test]
fn continuation_window_forbids_turn_end_closures() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut closed_request =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    closed_request.semantic_entries[3] = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_entry(35).id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ToolClosed {
            request: ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
        },
    );
    closed_request.steering_continuation_rounds =
        vec![SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            Vec::new(),
            Vec::new(),
        )];
    assert_eq!(
        assert_input_rejects_unchanged(closed_request.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// an ambiguous attempt end is a
/// turn-level failure and never reaches a continuation window.
#[test]
fn continuation_window_rejects_an_ambiguous_attempt_end() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut ambiguous_attempt =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    ambiguous_attempt.steering_continuation_rounds =
        vec![SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            vec![ended_tool_attempt_with_end(
                &session,
                active,
                matching_active_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
                ToolAttemptEnd::Ambiguous,
            )],
            Vec::new(),
        )];
    assert_eq!(
        assert_input_rejects_unchanged(ambiguous_attempt.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// a crash-lost attempt end is a
/// turn-level failure and never reaches a continuation window.
#[test]
fn continuation_window_rejects_a_crash_lost_attempt_end() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut crash_lost_attempt =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    crash_lost_attempt.steering_continuation_rounds =
        vec![SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            vec![ended_tool_attempt_with_end(
                &session,
                active,
                matching_active_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
                ToolAttemptEnd::KnownFailed {
                    error: ToolExecutionError::new(ToolExecutionErrorKind::CrashLost, None),
                },
            )],
            Vec::new(),
        )];
    assert_eq!(
        assert_input_rejects_unchanged(crash_lost_attempt.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// only a tool proposal keeps a completed
/// consumer's turn going, so a text-only completed consumer inside an
/// active turn cannot claim the historical-consumer correlation.
#[test]
fn text_only_completed_consumer_fails_closed() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut text_only_consumer =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    text_only_consumer.model_calls[0] = ModelCallReconstitutionInput::new(
        ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
        active.turn(),
        matching_active_attempt(),
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        frontier(41).id(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
    );
    text_only_consumer
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            semantic_entry(36).id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantText {
                producing_call: ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
                value: AssistantText::try_new(String::from("text-only response"))
                    .expect("fixture assistant text is valid"),
            },
        ));
    assert_eq!(
        assert_input_rejects_unchanged(text_only_consumer.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// a steering-consuming call that
/// completed by proposing a tool round stays reconstitutable while the
/// round is parked awaiting approval — the consumer is correlated through
/// its assistant history and exact frontier window, not the current
/// phase's attempt.
#[test]
fn parked_tool_round_retains_consumed_steering() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
    let steering_entry = semantic_entry(31);
    let tool_use_entry = semantic_entry(34);
    let call_frontier = frontier(41);
    let yielded_frontier = frontier(42);
    let consuming_call = ConsumedSteeringReconstitutionFacts::matching_continuation_call();
    let request_id = ConsumedSteeringReconstitutionFacts::matching_continuation_request();
    let request = ToolRequestReconstitutionInput::new(
        request_id,
        session.id(),
        active.turn(),
        consuming_call,
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("current_time")).expect("fixture name is canonical"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are canonical"),
    )
    .into_request();
    let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        yielded_frontier.id(),
        vec![
            origin_entry.reference(&session),
            steering_entry.reference(&session),
            tool_use_entry.reference(&session),
        ],
    )
    .expect("the tool response extends the steering-bearing call frontier");
    let batch = ToolBatchReconstitutionInput::new(
        session.id(),
        active.turn(),
        consuming_call,
        yielded,
        vec![request],
        vec![],
        vec![],
        ToolBatchPhaseReconstitutionInput::AwaitingApproval {
            request: request_id,
        },
    )
    .reconstitute()
    .expect("the undecided batch is awaiting approval");
    let mut facts = ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    let AcceptedInputTurnSchedulingRecordState::Active { phase, .. } = &mut facts.turns[0].state
    else {
        panic!("matching consumed-steering facts retain an active scheduling record");
    };
    *phase = ActiveTurnSchedulingReconstitutionInput::awaiting_approval(active.turn(), &batch)
        .expect("the approval wait names the parked batch");
    facts
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: consuming_call,
                request: request_id,
            },
        ));
    facts
        .snapshots
        .push(yielded_frontier.snapshot(&session, &[origin_entry, steering_entry, tool_use_entry]));
    facts.model_calls = vec![ModelCallReconstitutionInput::new(
        consuming_call,
        active.turn(),
        matching_active_attempt(),
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        call_frontier.id(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
    )];

    facts
        .input()
        .reconstitute()
        .expect("a parked tool round retains its consumed steering");
}
