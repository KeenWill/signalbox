//! Turn scheduling terminal frontier tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    OriginRecordFacts, accepted_origin, activation, current_session, default_origin_delivery,
    frontier, semantic_entry,
};
use super::*;

/// a live or startup-recovered completed response validates the
/// producing call's steering-extended source, stop provenance, and final
/// marker before the exact terminal frontier becomes the successor's
/// starting prefix.
#[test]
fn completed_frontier_becomes_successor_prefix() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let consumed = accepted_origin(2);
    let successor = accepted_origin(3);
    let origin_entry = semantic_entry(30);
    let steering_entry = semantic_entry(31);
    let assistant_entry = semantic_entry(32);
    let completion_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let completing_call = model_call_id(50);
    let activation = activation(2);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        predecessor.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        predecessor.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let interrupt_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: predecessor.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let assert_case = |completing_attempt_end, queued_record| {
        let terminal_record = predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                completing_attempt: turn_attempt_id(60),
                completing_attempt_end,
                completing_call,
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let steering = SemanticTranscriptEntryReconstitutionInput::new(
            steering_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: consumed.accepted_input(),
                source_turn: predecessor.turn(),
            },
        );
        let assistant = SemanticTranscriptEntryReconstitutionInput::new(
            assistant_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantText {
                producing_call: completing_call,
                value: AssistantText::try_new(String::from("reply"))
                    .expect("test assistant text is nonempty"),
            },
        );
        let completion = SemanticTranscriptEntryReconstitutionInput::new(
            completion_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCompleted {
                turn: predecessor.turn(),
            },
        );
        let call = ModelCallReconstitutionInput::new(
            completing_call,
            predecessor.turn(),
            turn_attempt_id(60),
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            call_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued_record, terminal_record],
            vec![
                assistant,
                completion,
                steering,
                predecessor.entry(&session, origin_entry),
            ],
            vec![
                terminal_frontier.snapshot(
                    &session,
                    &[
                        origin_entry,
                        steering_entry,
                        assistant_entry,
                        completion_entry,
                    ],
                ),
                call_frontier.snapshot(&session, &[origin_entry, steering_entry]),
                starting_frontier.snapshot(&session, &[origin_entry]),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                call.turn(),
                call.target(),
            )],
            vec![call],
        )
        .with_consumed_steering_facts(vec![ConsumedSteeringReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: completing_call,
                },
            ),
            consumed.position(),
            predecessor.turn(),
        )])
        .reconstitute()
        .expect("the completed predecessor is fully correlated");

        let collision = projection
            .clone()
            .prepare_earliest_queued_activation(
                activation.identities_with_attempt(turn_attempt_id(60)),
            )
            .expect_err("a terminal attempt identity cannot be minted again");
        assert_eq!(
            collision.failure(),
            AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists
        );

        let candidate = projection
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the completed predecessor releases the progressing slot");

        assert_eq!(candidate.turn().turn(), successor.turn());
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                origin_entry.reference(&session),
                steering_entry.reference(&session),
                assistant_entry.reference(&session),
                completion_entry.reference(&session),
                activation.origin_entry().reference(&session),
            ]
        );
    };

    assert_case(
        TerminalAttemptEndReconstitutionInput::without_stop(
            UnstoppedAttemptDisposition::TurnCompleted,
        ),
        successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::without_stop(UnstoppedAttemptDisposition::Lost),
        successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::after_cancellation(
            CancellationStopDisposition::TurnCompleted,
            interrupt,
        ),
        successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: interrupt_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::after_cancellation(
            CancellationStopDisposition::Lost,
            interrupt,
        ),
        successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: interrupt_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ),
    );
}

/// one physical attempt identity cannot
/// back terminal outcomes for two different turns.
#[test]
fn terminal_turns_reject_shared_attempt_identity() {
    let session = current_session();
    let completed = accepted_origin(1);
    let refused = accepted_origin(2);
    let completed_origin = semantic_entry(30);
    let assistant = semantic_entry(31);
    let completion = semantic_entry(32);
    let refused_origin = semantic_entry(33);
    let completed_start = frontier(40);
    let completed_terminal = frontier(41);
    let refused_start = frontier(42);
    let refused_terminal = frontier(43);
    let shared_attempt = turn_attempt_id(60);
    let completed_call = model_call_id(50);
    let refused_call = model_call_id(51);
    let completed_start_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        completed_start.id(),
        vec![completed_origin.reference(&session)],
    )
    .expect("the completed call frontier has unique membership");
    let refused_start_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        refused_start.id(),
        vec![
            completed_origin.reference(&session),
            assistant.reference(&session),
            completion.reference(&session),
            refused_origin.reference(&session),
        ],
    )
    .expect("the refused call frontier has unique membership");
    let completed_record = completed.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: completed_start.id(),
            completing_attempt: shared_attempt,
            completing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnCompleted,
            ),
            completing_call: completed_call,
            terminal_frontier: completed_terminal.id(),
        },
    );
    let refused_record = refused.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            starting_lineage: AcceptedInputStartingLineage::After {
                immediate_predecessor: completed.turn(),
            },
            starting_frontier: refused_start.id(),
            refusing_attempt: shared_attempt,
            refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnRefused,
            ),
            refusing_call: refused_call,
            terminal_frontier: refused_terminal.id(),
        },
    );
    let assistant_entry = SemanticTranscriptEntryReconstitutionInput::new(
        assistant.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::AssistantText {
            producing_call: completed_call,
            value: AssistantText::try_new("reply".to_owned())
                .expect("test assistant text is nonempty"),
        },
    );
    let completion_entry = SemanticTranscriptEntryReconstitutionInput::new(
        completion.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::TurnCompleted {
            turn: completed.turn(),
        },
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![refused_record, completed_record],
        vec![
            completed.entry(&session, completed_origin),
            assistant_entry,
            completion_entry,
            refused.entry(&session, refused_origin),
        ],
        vec![
            completed_start.snapshot(&session, &[completed_origin]),
            completed_terminal.snapshot(&session, &[completed_origin, assistant, completion]),
            refused_start.snapshot(
                &session,
                &[completed_origin, assistant, completion, refused_origin],
            ),
            refused_terminal.snapshot(
                &session,
                &[completed_origin, assistant, completion, refused_origin],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![
            crate::PinnedProviderTargetReconstitutionInput::new(
                completed.turn(),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
            ),
            crate::PinnedProviderTargetReconstitutionInput::new(
                refused.turn(),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
            ),
        ],
        vec![
            ModelCallReconstitutionInput::new(
                completed_call,
                completed.turn(),
                shared_attempt,
                FrozenModelSelection::Direct(direct(1)),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
                completed_start_snapshot.frontier().snapshot(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                refused_call,
                refused.turn(),
                shared_attempt,
                FrozenModelSelection::Direct(direct(1)),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
                refused_start_snapshot.frontier().snapshot(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
            ),
        ],
    );

    let error = input
        .reconstitute()
        .expect_err("one attempt cannot terminalize two turns");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
            attempt: shared_attempt,
        }
    );
}

/// a live or startup-recovered refusal validates the producing
/// call's steering-extended source and stop provenance, releases the slot,
/// and preserves its equal-content terminal frontier as the successor's
/// exact prefix.
#[test]
fn refused_frontier_becomes_successor_prefix() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let consumed = accepted_origin(2);
    let successor = accepted_origin(3);
    let origin_entry = semantic_entry(30);
    let steering_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let refusing_call = model_call_id(50);
    let refusing_attempt = turn_attempt_id(60);
    let activation = activation(2);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        predecessor.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        predecessor.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let interrupt_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: predecessor.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let assert_case = |refusing_attempt_end, queued_record| {
        let terminal_record = predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                refusing_attempt,
                refusing_attempt_end,
                refusing_call,
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let steering = SemanticTranscriptEntryReconstitutionInput::new(
            steering_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: consumed.accepted_input(),
                source_turn: predecessor.turn(),
            },
        );
        let call = ModelCallReconstitutionInput::new(
            refusing_call,
            predecessor.turn(),
            refusing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            call_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued_record, terminal_record],
            vec![predecessor.entry(&session, origin_entry), steering],
            vec![
                terminal_frontier.snapshot(&session, &[origin_entry, steering_entry]),
                call_frontier.snapshot(&session, &[origin_entry, steering_entry]),
                starting_frontier.snapshot(&session, &[origin_entry]),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                call.turn(),
                call.target(),
            )],
            vec![call],
        )
        .with_consumed_steering_facts(vec![ConsumedSteeringReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: refusing_call,
                },
            ),
            consumed.position(),
            predecessor.turn(),
        )])
        .reconstitute()
        .expect("the refused predecessor is fully correlated");

        let candidate = projection
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the refused predecessor releases the progressing slot");

        assert_eq!(candidate.turn().turn(), successor.turn());
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                origin_entry.reference(&session),
                steering_entry.reference(&session),
                activation.origin_entry().reference(&session),
            ]
        );
    };

    assert_case(
        TerminalAttemptEndReconstitutionInput::without_stop(
            UnstoppedAttemptDisposition::TurnRefused,
        ),
        successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::without_stop(UnstoppedAttemptDisposition::Lost),
        successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::after_cancellation(
            CancellationStopDisposition::TurnRefused,
            interrupt,
        ),
        successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: interrupt_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::after_cancellation(
            CancellationStopDisposition::Lost,
            interrupt,
        ),
        successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: interrupt_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ),
    );
}

/// assistant text cannot name a refused call because only
/// completed physical calls can produce semantic assistant content.
#[test]
fn refused_call_rejects_assistant_content() {
    let session = current_session();
    let origin = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let assistant_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let refusing_call = model_call_id(50);
    let refusing_attempt = turn_attempt_id(60);
    let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    )
    .expect("the call frontier has unique membership");
    let terminal_record = origin.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            refusing_attempt,
            refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnRefused,
            ),
            refusing_call,
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let assistant = SemanticTranscriptEntryReconstitutionInput::new(
        assistant_entry.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::AssistantText {
            producing_call: refusing_call,
            value: AssistantText::try_new(String::from("not a refusal"))
                .expect("test assistant text is nonempty"),
        },
    );
    let call = ModelCallReconstitutionInput::new(
        refusing_call,
        origin.turn(),
        refusing_attempt,
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        resolved_starting.frontier().snapshot(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![terminal_record],
        vec![assistant, origin.entry(&session, origin_entry)],
        vec![
            terminal_frontier.snapshot(&session, &[origin_entry]),
            starting_frontier.snapshot(&session, &[origin_entry]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            call.turn(),
            call.target(),
        )],
        vec![call],
    );

    let error = input
        .reconstitute()
        .expect_err("refused calls cannot produce assistant content");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::SemanticEntryCallMismatch {
            entry: assistant_entry.id(),
            call: refusing_call,
        }
    );
}

#[test]
fn refused_compaction_suffix_reconstitutes_exact_terminal_frontier() {
    let session = current_session();
    let origin = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let compaction_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let refusing_call = model_call_id(50);
    let refusing_attempt = turn_attempt_id(60);
    let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    )
    .expect("the call frontier has unique membership");
    let terminal_record = origin.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            refusing_attempt,
            refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnRefused,
            ),
            refusing_call,
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let compaction = SemanticTranscriptEntryReconstitutionInput::new(
        compaction_entry.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ProviderCompaction {
            producing_call: refusing_call,
            block: crate::ProviderCompactionBlock::try_new(String::from(
                r#"{"type":"compaction","content":"retained refusal summary"}"#,
            ))
            .expect("the fixture compaction block is valid"),
        },
    );
    let call = ModelCallReconstitutionInput::new(
        refusing_call,
        origin.turn(),
        refusing_attempt,
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        resolved_starting.frontier().snapshot(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
    );
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![terminal_record],
        vec![origin.entry(&session, origin_entry), compaction],
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(&session, &[origin_entry, compaction_entry]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            call.turn(),
            call.target(),
        )],
        vec![call],
    )
    .reconstitute()
    .expect("a refusal reload accepts its exact provider-compaction suffix");
}

/// a terminal refusal must be backed by the
/// stored ended-attempt refusal disposition, not only a matching identity.
#[test]
fn refused_turn_rejects_attempt_disposition_mismatch() {
    let session = current_session();
    let origin = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let refusing_call = model_call_id(50);
    let refusing_attempt = turn_attempt_id(60);
    let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    )
    .expect("the call frontier has unique membership");
    let terminal_record = origin.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            refusing_attempt,
            refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::KnownFailure,
            ),
            refusing_call,
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let call = ModelCallReconstitutionInput::new(
        refusing_call,
        origin.turn(),
        refusing_attempt,
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        resolved_starting.frontier().snapshot(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![terminal_record],
        vec![origin.entry(&session, origin_entry)],
        vec![
            terminal_frontier.snapshot(&session, &[origin_entry]),
            starting_frontier.snapshot(&session, &[origin_entry]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            call.turn(),
            call.target(),
        )],
        vec![call],
    );

    let error = input
        .reconstitute()
        .expect_err("a refusal cannot be inferred from attempt identity alone");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: origin.turn(),
        }
    );
}

/// a terminal-cancelled projection
/// validates the stored attempt end rather than inferring it from the
/// separately supplied interrupt result.
#[test]
fn cancelled_turn_rejects_attempt_end_mismatch() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let cancellation_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let attempt = turn_attempt_id(50);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let terminal_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                attempt,
                TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::Ambiguous,
                ),
                None,
                interrupt,
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: cancelled.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: successor_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let cancellation_entry = SemanticTranscriptEntryReconstitutionInput::new(
        cancellation_entry.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::TurnCancelled {
            turn: cancelled.turn(),
        },
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![terminal_record, successor_record],
        vec![cancelled.entry(&session, origin_entry), cancellation_entry],
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(&session, &[origin_entry, semantic_entry(31)]),
        ],
        None,
    );

    let error = input
        .reconstitute()
        .expect_err("cancelled turn authority cannot substitute for its attempt end");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
            turn: cancelled.turn(),
            attempt,
        }
    );
}

/// a cancelled call frontier must
/// preserve the starting frontier rather than substituting unrelated
/// semantic history before the cancellation marker.
#[test]
fn cancelled_turn_rejects_unrelated_call_frontier() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let cancellation_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let unrelated_call_frontier = frontier(42);
    let call = model_call_id(49);
    let attempt = turn_attempt_id(50);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let terminal_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                Some(call),
                interrupt,
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: cancelled.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: successor_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let cancellation_entry = SemanticTranscriptEntryReconstitutionInput::new(
        cancellation_entry.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::TurnCancelled {
            turn: cancelled.turn(),
        },
    );
    let stored_call = ModelCallReconstitutionInput::new(
        call,
        cancelled.turn(),
        attempt,
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        unrelated_call_frontier.id(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Cancelled),
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![terminal_record, successor_record],
        vec![cancelled.entry(&session, origin_entry), cancellation_entry],
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            unrelated_call_frontier.snapshot(&session, &[]),
            terminal_frontier.snapshot(&session, &[semantic_entry(31)]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
        )],
        vec![stored_call],
    );

    let error = input
        .reconstitute()
        .expect_err("a cancelled call cannot replace its turn's starting history");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: cancelled.turn(),
        }
    );
}

/// complete ambiguous-call facts reconstruct the
/// exact recovery wait and preserve the active progressing slot.
#[test]
fn ambiguous_call_reconstructs_recovery_wait() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let starting_frontier = frontier(40);
    let ambiguous_call = model_call_id(50);
    let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    )
    .expect("the call frontier has unique membership");
    let active_record = active.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            phase: ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery(
                active.turn(),
                turn_attempt_id(60),
                ambiguous_call,
            ),
        },
    );
    let call = ModelCallReconstitutionInput::new(
        ambiguous_call,
        active.turn(),
        turn_attempt_id(60),
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        resolved_starting.frontier().snapshot(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous),
    );
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![active_record],
        vec![active.entry(&session, origin_entry)],
        vec![starting_frontier.snapshot(&session, &[origin_entry])],
        Some(active.active_tail(&session)),
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            call.turn(),
            call.target(),
        )],
        vec![call],
    )
    .reconstitute()
    .expect("the ambiguous call and wait are fully correlated");
    let waiting = projection
        .active_turn()
        .expect("the recovery wait retains the progressing slot");

    assert!(matches!(
        waiting.active_phase(),
        Some(ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations,
            ..
        }) if ambiguous_operations.contains(crate::IssuedOperationRef::ModelCall(ambiguous_call))
    ));
}

/// an opaque wait from
/// a completely validated ambiguous tool batch reconstructs the exact
/// typed recovery subject and preserves it through interruption.
#[test]
fn interrupting_tool_recovery_preserves_exact_ambiguity() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let starting_frontier = frontier(40);
    let producing_call = model_call_id(50);
    let issuing_attempt = turn_attempt_id(60);
    let request = ToolRequestReconstitutionInput::new(
        tool_request_id(70),
        session.id(),
        active.turn(),
        producing_call,
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("external_tool")).expect("fixture name is canonical"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are canonical"),
    )
    .into_request();
    let approval = ToolApprovalResolutionReconstitutionInput::user_fixture(
        request.id(),
        ToolApprovalDecision::Approve,
    )
    .reconstitute()
    .expect("user approval is implemented");
    let expected_tool_attempt = tool_attempt_id(80);
    let tool_attempt = ToolAttemptReconstitutionInput::new(
        expected_tool_attempt,
        request.id(),
        session.id(),
        active.turn(),
        issuing_attempt,
        ToolEffectClass::ExternalEffect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    )
    .expect("the yielded snapshot is valid");
    let expected_request = request.id();
    let batch = ToolBatchReconstitutionInput::new(
        session.id(),
        active.turn(),
        producing_call,
        yielded,
        vec![request],
        vec![approval],
        vec![tool_attempt],
        ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
            attempt: expected_tool_attempt,
        },
    )
    .reconstitute()
    .expect("the complete tool batch is exactly ambiguous");
    let wait = batch
        .awaiting_recovery()
        .expect("the validated batch exposes opaque wait evidence");
    let cross_wired_record = active.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            phase: ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery(
                active.turn(),
                turn_attempt_id(61),
                wait,
            ),
        },
    );
    let error = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cross_wired_record],
        vec![active.entry(&session, origin_entry)],
        vec![starting_frontier.snapshot(&session, &[origin_entry])],
        Some(active.active_tail(&session)),
    )
    .reconstitute()
    .expect_err("the wait cannot be attached to another turn attempt");
    let AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch { turn, .. } =
        error.failure()
    else {
        panic!("the cross-wired wait fails as an active-phase mismatch");
    };
    assert_eq!(*turn, active.turn());
    let successor = accepted_origin(2);
    let successor_order =
        AcceptedInputQueueOrder::interrupt_immediately_after(successor.position(), active.turn());
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(90),
        session.id(),
        active.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let terminal_tool_reconciliation = active.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            reconciling_attempt: issuing_attempt,
            reconciling_attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                CancellationStopDisposition::Lost,
                interrupt,
            ),
            tool_batch: batch.clone(),
            authority: AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
            terminal_frontier: starting_frontier.id(),
        },
    );
    assert!(
        scheduling_record_is_terminal(&terminal_tool_reconciliation),
        "tool reconciliation is terminal historical proof"
    );
    let active_record = active.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                phase:
                    ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery_after_cancellation_restart(
                    active.turn(),
                    issuing_attempt,
                    wait,
                    interrupt,
                ),
            },
        );
    let successor_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: active.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: successor_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let acceptance_tail = SessionAcceptanceTailReconstitutionInput::new(
        session.id(),
        active.accepted_input(),
        successor.position(),
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
                    successor.accepted_input(),
                    AcceptedInputDisposition::OriginOf(successor.turn()),
                ),
                successor.position(),
                successor_delivery,
            ),
        ],
    );
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![active_record, successor_record],
        vec![active.entry(&session, origin_entry)],
        vec![starting_frontier.snapshot(&session, &[origin_entry])],
        Some(acceptance_tail),
    )
    .reconstitute()
    .expect("the opaque tool wait and ended turn attempt are correlated");
    let waiting = projection
        .active_turn()
        .expect("the recovery wait retains the progressing slot");

    let Some(ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        ..
    }) = waiting.active_phase()
    else {
        panic!("the opaque tool wait remains an active recovery decision");
    };
    assert!(
        ambiguous_operations.contains(crate::IssuedOperationRef::ToolAttempt(
            expected_tool_attempt
        ))
    );

    let retained_request = batch
        .requests()
        .first()
        .expect("the one-request batch retains its request");
    assert_eq!(retained_request.id(), expected_request);
    let Some(crate::ReconstitutedToolAttempt::Ended(ended_tool)) =
        batch.attempt(retained_request.id())
    else {
        panic!("the batch retains its ended ambiguous attempt");
    };
    assert_eq!(ended_tool.attempt(), expected_tool_attempt);
    let ended_tool = ended_tool.clone();
    let result_entry = semantic_entry(31);
    let result_projection = batch
        .prepare_reconciliation_projection(vec![result_entry.id()], frontier(41).id())
        .expect("the terminal batch closes its logical request");
    let reconciled = projection
        .apply_interrupt_to_tool_recovery(
            wait,
            ended_tool,
            result_projection,
            interrupt,
            crate::AmbiguousModelCallTurnIdentities::new(frontier(41).id()),
        )
        .expect("the interrupt retains exact tool ambiguity");
    assert_eq!(reconciled.tool_attempt().attempt(), expected_tool_attempt);
    assert_eq!(
        reconciled.attempt().end(),
        &AttemptEnd::AfterCancellation {
            cause: interrupt.proof(),
            disposition: CancellationStopDisposition::Lost,
        }
    );
    assert_eq!(
        reconciled
            .terminal_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            origin_entry.reference(&session),
            result_entry.reference(&session)
        ]
    );
    assert_eq!(
        reconciled.tool_result_entries()[0].payload(),
        &crate::SemanticTranscriptEntryPayload::ToolClosed {
            request: expected_request,
        }
    );
    let crate::TurnDisposition::ReconciliationRequired { marker } = reconciled.disposition() else {
        panic!("the interrupted ambiguity requires reconciliation");
    };
    assert!(
        marker
            .ambiguous_operations()
            .contains(crate::IssuedOperationRef::ToolAttempt(
                expected_tool_attempt
            ))
    );
}

/// a later interrupt supplies terminal authority
/// without being rewritten into an already ambiguous attempt end.
#[test]
fn tool_reconciliation_retains_without_stop_attempt_end() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let successor = accepted_origin(2);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        predecessor.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(90),
        session.id(),
        predecessor.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let attempt_end =
        TerminalAttemptEndReconstitutionInput::without_stop(UnstoppedAttemptDisposition::Ambiguous);

    assert!(tool_reconciliation_attempt_end_matches(
        &attempt_end,
        Some(interrupt),
    ));
}

/// a predecessor snapshot that omits its required failed
/// marker is not a terminal frontier and cannot authorize a successor.
#[test]
fn incomplete_failed_terminal_frontier_fails_closed() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let failure_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let no_active_acceptance_tail = None;
    let error = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: None,
                terminal_frontier: terminal_frontier.id(),
            },
        )],
        vec![
            predecessor.entry(&session, origin_entry),
            failure_entry.failed_turn(&session, predecessor),
        ],
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(&session, &[origin_entry]),
        ],
        no_active_acceptance_tail,
    )
    .reconstitute()
    .expect_err("the failed marker must follow the exact starting prefix");

    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
            turn: predecessor.turn(),
        }
    );
}
