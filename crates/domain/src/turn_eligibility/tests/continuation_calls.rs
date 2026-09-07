//! Turn scheduling continuation calls tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::fixtures::{
    OriginFixture, OriginRecordFacts, accepted_origin, assert_input_rejects_unchanged,
    current_session, ended_tool_attempt, frontier, semantic_entry,
};
use super::*;

/// Matching stored facts for one failed terminal turn naming its
/// round-two continuation call: the call's whole frontier is the
/// completed round's result projection and the terminal frontier extends
/// it by exactly the failure marker.
fn failed_continuation_call_input(
    session: &Session,
    failed: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let session = session.clone();
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let failure_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let continuation_call = model_call_id(53);
    let request = tool_request_id(60);
    let executed_attempt = ended_tool_attempt(
        &session,
        failed,
        terminal_attempt,
        tool_attempt_id(70),
        request,
    );
    let failed_record = failed.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: Some(
                FailedTurnExecutionReconstitutionInput::with_call(
                    failed.turn(),
                    terminal_attempt,
                    UnstoppedAttemptDisposition::KnownFailure,
                    continuation_call,
                )
                .with_terminal_tool_attempts(vec![executed_attempt]),
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let semantic_entries = vec![
        failed.entry(&session, origin_entry),
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
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
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
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![failed_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
            terminal_frontier.snapshot(
                &session,
                &[origin_entry, tool_use_entry, result_entry, failure_entry],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            failed.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                failed.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                continuation_call,
                failed.turn(),
                terminal_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::KnownFailed),
            ),
        ],
    )
}

/// a failed terminal turn naming its round-two
/// continuation call reconstitutes when that call's whole frontier is the
/// completed round's result projection the terminal marker extends.
#[test]
fn failed_continuation_call_reconstitutes() {
    let session = current_session();
    let failed = accepted_origin(1);
    failed_continuation_call_input(&session, failed)
        .reconstitute()
        .expect("the failed continuation-call terminal shape reconstructs");
}

/// a failed terminal turn naming a
/// continuation call is accepted only with its round's result evidence.
#[test]
fn failed_continuation_call_requires_round_evidence() {
    let session = current_session();
    let failed = accepted_origin(1);
    let mut missing_evidence = failed_continuation_call_input(&session, failed);
    let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
        terminal_execution: Some(execution),
        ..
    } = &mut missing_evidence.turns[0].state
    else {
        panic!("fixture is a failed terminal");
    };
    execution.terminal_tool_attempts = Vec::new();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: failed.turn(),
        }
    );
}

/// a named continuation call's round
/// completed, so its window forbids turn-end closures.
#[test]
fn failed_continuation_call_window_forbids_turn_end_closures() {
    let session = current_session();
    let failed = accepted_origin(1);
    let mut closed_request = failed_continuation_call_input(&session, failed);
    closed_request.semantic_entries[2] = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_entry(32).id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ToolClosed {
            request: tool_request_id(60),
        },
    );
    let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
        terminal_execution: Some(execution),
        ..
    } = &mut closed_request.turns[0].state
    else {
        panic!("fixture is a failed terminal");
    };
    execution.terminal_tool_attempts = Vec::new();
    assert_eq!(
        assert_input_rejects_unchanged(closed_request),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: failed.turn(),
        }
    );
}

/// Matching stored facts for one cancelled terminal turn naming its
/// unsent round-two continuation call: the call's whole frontier is the
/// completed round's result projection and the terminal frontier extends
/// it by exactly the cancellation marker.
fn cancelled_continuation_call_input(
    session: &Session,
    cancelled: OriginFixture,
    successor: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let session = session.clone();
    let origin_entry = semantic_entry(30);
    let compaction_entry = semantic_entry(34);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let continuation_call = model_call_id(53);
    let request = tool_request_id(60);
    let executed_attempt = ended_tool_attempt(
        &session,
        cancelled,
        terminal_attempt,
        tool_attempt_id(70),
        request,
    );
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(71),
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
                Some(continuation_call),
                interrupt,
            )
            .with_terminal_tool_attempts(vec![executed_attempt]),
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
            compaction_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call,
                block: crate::ProviderCompactionBlock::try_new(String::from(
                    r#"{"type":"compaction","content":"retained summary"}"#,
                ))
                .expect("fixture provider compaction is valid"),
            },
        ),
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
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
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
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            call_frontier.snapshot(
                &session,
                &[origin_entry, compaction_entry, tool_use_entry, result_entry],
            ),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    compaction_entry,
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
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                cancelled.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                continuation_call,
                cancelled.turn(),
                terminal_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Cancelled),
            ),
        ],
    )
}

/// a cancelled terminal turn naming
/// its unsent round-two continuation call reconstitutes when provider
/// compaction precedes the tool proposal and that call's whole frontier is
/// the completed round's result projection the cancellation marker
/// extends.
#[test]
fn cancelled_continuation_call_reconstitutes() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    cancelled_continuation_call_input(&session, cancelled, successor)
        .reconstitute()
        .expect("the cancelled continuation-call terminal shape reconstructs");
}

/// a cancelled terminal turn naming
/// a continuation call is accepted only with its round's result evidence.
#[test]
fn cancelled_continuation_call_requires_round_evidence() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let mut missing_evidence = cancelled_continuation_call_input(&session, cancelled, successor);
    let AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
        terminal_execution, ..
    } = &mut missing_evidence.turns[0].state
    else {
        panic!("fixture is a cancelled terminal");
    };
    terminal_execution.terminal_tool_attempts = Vec::new();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: cancelled.turn(),
        }
    );
}

/// Matching stored facts for one refused terminal turn naming its
/// round-two continuation call: the call's whole frontier is the
/// completed round's result projection and the equal-content terminal
/// frontier extends it by no entry.
fn refused_continuation_call_input(
    session: &Session,
    refused: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let session = session.clone();
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let continuation_call = model_call_id(53);
    let request = tool_request_id(60);
    let executed_attempt = ended_tool_attempt(
        &session,
        refused,
        terminal_attempt,
        tool_attempt_id(70),
        request,
    );
    let refused_record = refused.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            refusing_attempt: terminal_attempt,
            refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnRefused,
            ),
            refusing_call: continuation_call,
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let semantic_entries = vec![
        refused.entry(&session, origin_entry),
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
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![refused_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
            terminal_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            refused.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                refused.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                continuation_call,
                refused.turn(),
                terminal_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
            ),
        ],
    )
    .with_continuation_rounds(vec![ContinuationRoundReconstitutionInput::new(
        continuation_call,
        vec![executed_attempt],
        Vec::new(),
    )])
}

/// a refused terminal turn naming its round-two
/// continuation call reconstitutes when that call's whole frontier is the
/// completed round's result projection the equal-content terminal
/// frontier repeats.
#[test]
fn refused_continuation_call_reconstitutes() {
    let session = current_session();
    let refused = accepted_origin(1);
    refused_continuation_call_input(&session, refused)
        .reconstitute()
        .expect("the refused continuation-call terminal shape reconstructs");
}

/// a refused terminal turn naming a continuation
/// call is accepted only with its round's result evidence.
#[test]
fn refused_continuation_call_requires_round_evidence() {
    let session = current_session();
    let refused = accepted_origin(1);
    let mut missing_evidence = refused_continuation_call_input(&session, refused);
    missing_evidence.continuation_rounds.clear();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: refused.turn(),
        }
    );
}

/// a named refused continuation call's round
/// completed, so its window forbids turn-end closures.
#[test]
fn refused_continuation_call_window_forbids_turn_end_closures() {
    let session = current_session();
    let refused = accepted_origin(1);
    let mut closed_request = refused_continuation_call_input(&session, refused);
    closed_request.semantic_entries[2] = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_entry(32).id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ToolClosed {
            request: tool_request_id(60),
        },
    );
    closed_request.continuation_rounds = vec![ContinuationRoundReconstitutionInput::new(
        model_call_id(53),
        Vec::new(),
        Vec::new(),
    )];
    assert_eq!(
        assert_input_rejects_unchanged(closed_request),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: refused.turn(),
        }
    );
}

/// gate-named continuation-round evidence names each
/// call at most once.
#[test]
fn continuation_round_evidence_names_each_call_once() {
    let session = current_session();
    let refused = accepted_origin(1);
    let mut duplicate_evidence = refused_continuation_call_input(&session, refused);
    let duplicated = duplicate_evidence.continuation_rounds[0].clone();
    duplicate_evidence.continuation_rounds.push(duplicated);
    assert_eq!(
        assert_input_rejects_unchanged(duplicate_evidence),
        AcceptedInputSchedulingReconstitutionFailure::ContinuationRoundMismatch {
            call: model_call_id(53),
        }
    );
}

/// gate-named continuation-round evidence must name
/// a call a terminal or recovery gate proves against it.
#[test]
fn continuation_round_evidence_requires_a_naming_gate() {
    let session = current_session();
    let refused = accepted_origin(1);
    let mut dangling_evidence = refused_continuation_call_input(&session, refused);
    dangling_evidence
        .continuation_rounds
        .push(ContinuationRoundReconstitutionInput::new(
            model_call_id(50),
            Vec::new(),
            Vec::new(),
        ));
    assert_eq!(
        assert_input_rejects_unchanged(dangling_evidence),
        AcceptedInputSchedulingReconstitutionFailure::ContinuationRoundMismatch {
            call: model_call_id(50),
        }
    );
}

/// Matching stored facts for one reconciliation-required terminal turn
/// naming its interrupted round-two continuation call: the ambiguous
/// call's whole frontier is the completed round's result projection and
/// the equal-content terminal frontier extends it by no entry.
fn reconciliation_required_continuation_call_input(
    session: &Session,
    reconciled: OriginFixture,
    successor: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let session = session.clone();
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let continuation_call = model_call_id(53);
    let request = tool_request_id(60);
    let executed_attempt = ended_tool_attempt(
        &session,
        reconciled,
        terminal_attempt,
        tool_attempt_id(70),
        request,
    );
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        reconciled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(71),
        session.id(),
        reconciled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the reconciling interrupt is exactly correlated");
    let reconciled_record = reconciled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            reconciling_attempt: terminal_attempt,
            reconciling_attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                CancellationStopDisposition::Lost,
                interrupt,
            ),
            ambiguous_call: continuation_call,
            authority: AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: reconciled.turn(),
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
        reconciled.entry(&session, origin_entry),
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
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![reconciled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
            terminal_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            reconciled.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                reconciled.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                continuation_call,
                reconciled.turn(),
                terminal_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous),
            ),
        ],
    )
    .with_continuation_rounds(vec![ContinuationRoundReconstitutionInput::new(
        continuation_call,
        vec![executed_attempt],
        Vec::new(),
    )])
}

/// a reconciliation-required terminal turn
/// naming its interrupted round-two continuation call reconstitutes when
/// that call's whole frontier is the completed round's result projection
/// the equal-content terminal frontier repeats.
#[test]
fn reconciliation_required_continuation_call_reconstitutes() {
    let session = current_session();
    let reconciled = accepted_origin(1);
    let successor = accepted_origin(2);
    reconciliation_required_continuation_call_input(&session, reconciled, successor)
        .reconstitute()
        .expect("the reconciliation-required continuation-call terminal shape reconstructs");
}

/// a reconciliation-required terminal turn
/// naming a continuation call is accepted only with its round's result
/// evidence.
#[test]
fn reconciliation_required_continuation_call_requires_round_evidence() {
    let session = current_session();
    let reconciled = accepted_origin(1);
    let successor = accepted_origin(2);
    let mut missing_evidence =
        reconciliation_required_continuation_call_input(&session, reconciled, successor);
    missing_evidence.continuation_rounds.clear();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: reconciled.turn(),
        }
    );
}

/// Matching stored facts for one active turn parked on the ambiguous
/// round-two continuation call of a completed tool round: the call's
/// whole frontier is the completed round's result projection and the
/// recovery wait extends it by no entry.
fn recovery_wait_continuation_call_input(
    session: &Session,
    active: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let session = session.clone();
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let recovery_attempt = turn_attempt_id(52);
    let continuation_call = model_call_id(53);
    let request = tool_request_id(60);
    let executed_attempt = ended_tool_attempt(
        &session,
        active,
        recovery_attempt,
        tool_attempt_id(70),
        request,
    );
    let active_record = active.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            phase:
                ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_restart(
                    active.turn(),
                    recovery_attempt,
                    continuation_call,
                ),
        },
    );
    let semantic_entries = vec![
        active.entry(&session, origin_entry),
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
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![active_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
        ],
        Some(active.active_tail(&session)),
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            active.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                active.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                continuation_call,
                active.turn(),
                recovery_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous),
            ),
        ],
    )
    .with_continuation_rounds(vec![ContinuationRoundReconstitutionInput::new(
        continuation_call,
        vec![executed_attempt],
        Vec::new(),
    )])
}

/// an active turn parked on the ambiguous
/// round-two continuation call of a completed tool round reconstitutes
/// the exact recovery wait when that call's whole frontier is the
/// completed round's result projection.
#[test]
fn recovery_wait_continuation_call_reconstitutes() {
    let session = current_session();
    let active = accepted_origin(1);
    let projection = recovery_wait_continuation_call_input(&session, active)
        .reconstitute()
        .expect("the parked continuation-call recovery wait reconstructs");
    let waiting = projection
        .active_turn()
        .expect("the recovery wait retains the progressing slot");

    assert!(matches!(
        waiting.active_phase(),
        Some(ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations,
            ..
        }) if ambiguous_operations
            .contains(crate::IssuedOperationRef::ModelCall(model_call_id(53)))
    ));
}

/// a recovery wait naming a continuation call is
/// accepted only with its round's result evidence.
#[test]
fn recovery_wait_continuation_call_requires_round_evidence() {
    let session = current_session();
    let active = accepted_origin(1);
    let mut missing_evidence = recovery_wait_continuation_call_input(&session, active);
    missing_evidence.continuation_rounds.clear();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence),
        AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
            turn: active.turn(),
        }
    );
}
