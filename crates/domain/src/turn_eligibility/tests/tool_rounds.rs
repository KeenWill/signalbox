//! Turn scheduling tool rounds tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    ActiveReconstitutionFacts, OriginRecordFacts, accepted_origin, assert_input_rejects_unchanged,
    current_session, frontier, semantic_entry, user_denial,
};
use super::*;

/// scheduling
/// reconstitution accepts the exact terminal shape written when an
/// interrupt closes a yielded tool round.
#[test]
fn cancelled_tool_round_reconstitutes() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let request = tool_request_id(60);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(70),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                terminal_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                None,
                interrupt,
            )
            .with_terminal_tool_denials(vec![user_denial(request)]),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolDenied { request },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    tool_use_entry,
                    result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            cancelled.turn(),
            producing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            target,
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    );

    let projection = input
        .reconstitute()
        .expect("the writer-produced cancelled tool-round shape reconstitutes");

    assert_eq!(
        projection
            .turn(cancelled.turn())
            .expect("the cancelled turn remains present")
            .status(),
        AcceptedInputTurnSchedulingStatus::TerminalCancelled
    );
    assert_eq!(
        projection
            .earliest_queued_turn()
            .expect("the interrupt successor remains queued")
            .turn(),
        successor.turn()
    );
}

/// scheduling
/// reconstitution accepts the exact terminal shape written when a stop
/// request races a tool-using response, which names the batch's completed
/// producing call.
#[test]
fn stopped_tool_round_reconstitutes_from_named_call() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let closed_result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    // The stop-requested attempt that issued the racing call is the exact
    // attempt the cancellation ends, so one identity names both.
    let stopped_attempt = turn_attempt_id(51);
    let request = tool_request_id(60);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(70),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                stopped_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                Some(producing_call),
                interrupt,
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            closed_result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed { request },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    tool_use_entry,
                    closed_result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            cancelled.turn(),
            stopped_attempt,
            FrozenModelSelection::Direct(direct(1)),
            target,
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    );

    let projection = input
        .reconstitute()
        .expect("the writer-produced stopped tool-round shape reconstitutes");

    assert_eq!(
        projection
            .turn(cancelled.turn())
            .expect("the cancelled turn remains present")
            .status(),
        AcceptedInputTurnSchedulingStatus::TerminalCancelled
    );
    assert_eq!(
        projection
            .earliest_queued_turn()
            .expect("the interrupt successor remains queued")
            .turn(),
        successor.turn()
    );
}

/// a cancelled terminal
/// turn naming a completed call that is not the tool round's producing
/// call fails closed.
#[test]
fn cancelled_tool_round_rejects_unrelated_named_call() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let closed_result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    // The named call is a completed call of the same turn and attempt that
    // proposed nothing in the terminal round: naming it is the behavior
    // under test.
    let unrelated_call = model_call_id(51);
    let stopped_attempt = turn_attempt_id(52);
    let request = tool_request_id(60);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(70),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                stopped_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                Some(unrelated_call),
                interrupt,
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            closed_result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed { request },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    tool_use_entry,
                    closed_result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                cancelled.turn(),
                stopped_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                unrelated_call,
                cancelled.turn(),
                stopped_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
        ],
    );

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
            turn: cancelled.turn(),
        }
    );
}

/// a cancelled terminal
/// tool round whose `ToolDenied` result entry names no user denial
/// resolution fails closed.
#[test]
fn cancelled_tool_round_rejects_missing_denial_resolution() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let request = tool_request_id(60);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(70),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                terminal_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                None,
                interrupt,
            )
            // The denial entry's backing user resolution is deliberately
            // absent: this emptiness is the behavior under test.
            .with_terminal_tool_denials(Vec::new()),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolDenied { request },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    tool_use_entry,
                    result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            cancelled.turn(),
            producing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            target,
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    );

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
            turn: cancelled.turn(),
        }
    );
}

/// an approving user
/// resolution cannot back a cancelled terminal tool round's `ToolDenied`
/// result entry; the round fails closed.
#[test]
fn cancelled_tool_round_rejects_approving_resolution() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let request = tool_request_id(60);
    let approving_resolution = ToolApprovalResolutionReconstitutionInput::user_fixture(
        request,
        ToolApprovalDecision::Approve,
    )
    .reconstitute()
    .expect("the approving resolution fixture is valid");
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(70),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                terminal_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                None,
                interrupt,
            )
            .with_terminal_tool_denials(vec![approving_resolution]),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolDenied { request },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    tool_use_entry,
                    result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            cancelled.turn(),
            producing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            target,
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    );

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
            turn: cancelled.turn(),
        }
    );
}

/// scheduling
/// reconstitution accepts the exact terminal shape written when a
/// crash-lost tool round closes the turn as failed.
#[test]
fn failed_tool_round_reconstitutes() {
    let session = current_session();
    let failed = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let first_tool_use = semantic_entry(31);
    let second_tool_use = semantic_entry(32);
    let first_result = semantic_entry(33);
    let second_result = semantic_entry(34);
    let failure_entry = semantic_entry(35);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let first_request = tool_request_id(60);
    let second_request = tool_request_id(61);
    let executed_attempt = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(70),
        first_request,
        session.id(),
        failed.turn(),
        terminal_attempt,
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(
                ToolResultText::try_new(String::from("ok")).expect("fixture tool result is valid"),
            ),
        }),
    )
    .reconstitute()
    .expect("fixture tool attempt is supported");
    let crate::ReconstitutedToolAttempt::Ended(executed_attempt) = executed_attempt else {
        panic!("fixture tool attempt is terminal");
    };
    let failed_record = failed.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: Some(
                FailedTurnExecutionReconstitutionInput::attempt_only(
                    failed.turn(),
                    terminal_attempt,
                    UnstoppedAttemptDisposition::KnownFailure,
                )
                .with_terminal_tool_attempts(vec![executed_attempt]),
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let semantic_entries = vec![
        failed.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            first_tool_use.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request: first_request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            second_tool_use.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request: second_request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            first_result.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            second_result.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed {
                request: second_request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            failure_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnFailed {
                turn: failed.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![failed_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    first_tool_use,
                    second_tool_use,
                    first_result,
                    second_result,
                    failure_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            failed.turn(),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            failed.turn(),
            producing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            target,
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    );

    let mismatched_attempt = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(70),
        second_request,
        session.id(),
        failed.turn(),
        terminal_attempt,
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(
                ToolResultText::try_new(String::from("wrong request"))
                    .expect("fixture tool result is valid"),
            ),
        }),
    )
    .reconstitute()
    .expect("fixture tool attempt is supported");
    let crate::ReconstitutedToolAttempt::Ended(mismatched_attempt) = mismatched_attempt else {
        panic!("fixture tool attempt is terminal");
    };
    let mut mismatched_input = input.clone();
    let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
        terminal_execution: Some(execution),
        ..
    } = &mut mismatched_input.turns[0].state
    else {
        panic!("fixture is a failed terminal");
    };
    execution.terminal_tool_attempts = vec![mismatched_attempt];
    assert_eq!(
        mismatched_input
            .reconstitute()
            .expect_err("a result attempt must execute its paired request")
            .failure()
            .to_owned(),
        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
            turn: failed.turn(),
        }
    );

    let projection = input
        .reconstitute()
        .expect("the writer-produced failed tool-round shape reconstitutes");

    assert_eq!(
        projection
            .turn(failed.turn())
            .expect("the failed turn remains present")
            .status(),
        AcceptedInputTurnSchedulingStatus::TerminalFailed
    );
}

/// complete scheduling reconstitution admits every
/// reference-only tool entry while retaining completed-call provenance
/// for assistant tool use from an earlier intra-turn round.
#[test]
fn scheduling_reconstitutes_tool_round_history() {
    let session = current_session();
    let active = accepted_origin(1);
    let producing_call = model_call_id(90);
    let request = tool_request_id(91);
    let attempt = tool_attempt_id(92);
    let denied_request = tool_request_id(93);
    let closed_request = tool_request_id(94);
    let tool_use = semantic_entry(31);
    let execution_result = semantic_entry(32);
    let denied = semantic_entry(33);
    let closed = semantic_entry(34);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.semantic_entries.extend([
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            execution_result.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult { attempt },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            denied.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolDenied {
                request: denied_request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            closed.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed {
                request: closed_request,
            },
        ),
    ]);
    let target = ResolvedProviderTarget::naming(provider_model_identity(51));
    let projection = facts
        .input()
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                active.turn(),
                target,
            )],
            vec![ModelCallReconstitutionInput::new(
                producing_call,
                active.turn(),
                turn_attempt_id(49),
                FrozenModelSelection::Direct(direct(1)),
                target,
                ActiveReconstitutionFacts::matching_starting_frontier().id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            )],
        )
        .reconstitute()
        .expect("tool-round history and its completed producing call agree");

    assert!(matches!(
        projection
            .semantic_entry(tool_use.reference(&session))
            .map(SemanticTranscriptEntry::payload),
        Some(InitialSemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: actual_call,
            request: actual_request,
        }) if *actual_call == producing_call && *actual_request == request
    ));
    assert!(matches!(
        projection
            .semantic_entry(execution_result.reference(&session))
            .map(SemanticTranscriptEntry::payload),
        Some(InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
            attempt: actual,
        }) if *actual == attempt
    ));
    assert!(matches!(
        projection
            .semantic_entry(denied.reference(&session))
            .map(SemanticTranscriptEntry::payload),
        Some(InitialSemanticTranscriptEntryPayload::ToolDenied { request: actual })
            if *actual == denied_request
    ));
    assert!(matches!(
        projection
            .semantic_entry(closed.reference(&session))
            .map(SemanticTranscriptEntry::payload),
        Some(InitialSemanticTranscriptEntryPayload::ToolClosed { request: actual })
            if *actual == closed_request
    ));
}
