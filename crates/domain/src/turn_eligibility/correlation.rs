//! Turn scheduling correlation for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::failure::AcceptedInputSchedulingReconstitutionFailure;
use super::reconstitution_input::TerminalAttemptEndReconstitutionInput;
use super::record::{
    AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState,
    AutomaticReconciliationAuthority,
};
use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputQueuePriority,
    AcceptedInputStartingLineage, AcceptedInputTurnStart, AppliedInterruptCommandResult,
    AttemptEnd, CancellationStopDisposition, ContextFrontierId, DeliveryRequest,
    InitialSemanticTranscriptEntryPayload, ModelCallDisposition, OriginConfiguration,
    ReconstitutedModelCall, ResolvedContextFrontierSnapshot, SemanticTranscriptEntry,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef, SessionId, ToolApprovalDecision,
    ToolApprovalResolution, TurnConfigurationProvenance, TurnId, UnstoppedAttemptDisposition,
};
use std::{collections::BTreeMap, collections::BTreeSet};

pub(super) fn scheduling_record_is_terminal(record: &AcceptedInputTurnSchedulingRecord) -> bool {
    matches!(
        &record.state,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed { .. }
            | AcceptedInputTurnSchedulingRecordState::TerminalCompleted { .. }
            | AcceptedInputTurnSchedulingRecordState::TerminalRefused { .. }
            | AcceptedInputTurnSchedulingRecordState::TerminalCancelled { .. }
            | AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired { .. }
            | AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired { .. }
    )
}

pub(super) fn historical_interrupt_matches_terminal_proof(
    session: SessionId,
    active: TurnId,
    expected_active_turn: TurnId,
    accepted_input: AcceptedInputId,
    successor: &AcceptedInputTurnSchedulingRecord,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
) -> bool {
    expected_active_turn != active
        && scheduling_record_is_terminal(successor)
        && records_by_turn
            .get(&expected_active_turn)
            .filter(|predecessor| scheduling_record_is_terminal(predecessor))
            .and_then(|predecessor| terminal_record_interrupt(predecessor))
            .is_some_and(|interrupt| {
                interrupt.session() == session
                    && interrupt.proof().predecessor() == expected_active_turn
                    && interrupt.accepted_input() == accepted_input
                    && interrupt.successor() == successor.turn
                    && interrupt.successor_order() == successor.order
            })
}

fn terminal_record_interrupt(
    record: &AcceptedInputTurnSchedulingRecord,
) -> Option<AppliedInterruptCommandResult> {
    match &record.state {
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            terminal_execution: Some(execution),
            ..
        } => execution.attempt_end.interrupt(),
        AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
            completing_attempt_end,
            ..
        } => completing_attempt_end.interrupt(),
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            refusing_attempt_end,
            ..
        } => refusing_attempt_end.interrupt(),
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            terminal_execution, ..
        } => Some(terminal_execution.interrupt),
        AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
            authority,
            ..
        } => match authority {
            AutomaticReconciliationAuthority::AppliedInterrupt(interrupt) => Some(*interrupt),
            AutomaticReconciliationAuthority::AutomaticRecovery { .. } => None,
        },
        AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
            authority,
            ..
        } => match authority {
            AutomaticReconciliationAuthority::AppliedInterrupt(interrupt) => Some(*interrupt),
            AutomaticReconciliationAuthority::AutomaticRecovery { .. } => None,
        },
        AcceptedInputTurnSchedulingRecordState::Queued
        | AcceptedInputTurnSchedulingRecordState::Active { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            terminal_execution: None,
            ..
        } => None,
    }
}

pub(super) fn tool_reconciliation_attempt_end_matches(
    attempt_end: &TerminalAttemptEndReconstitutionInput,
    interrupt: Option<AppliedInterruptCommandResult>,
) -> bool {
    match attempt_end.end() {
        AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost,
        } => attempt_end.interrupt().is_none(),
        AttemptEnd::AfterCancellation {
            cause,
            disposition: CancellationStopDisposition::Ambiguous | CancellationStopDisposition::Lost,
        } => {
            interrupt.is_some_and(|interrupt| *cause == interrupt.proof())
                && attempt_end.interrupt() == interrupt
        }
        AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::YieldedToDurableWait,
        } => interrupt.is_some_and(|interrupt| attempt_end.interrupt() == Some(interrupt)),
        _ => false,
    }
}

pub(super) fn origin_delivery_matches_record(
    delivery: DeliveryRequest,
    record: &AcceptedInputTurnSchedulingRecord,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
    preceding_non_accepted_terminals: &BTreeSet<TurnId>,
) -> bool {
    if let TurnConfigurationProvenance::InheritedForReclassifiedSteering(binding) =
        &record.configuration_provenance
    {
        let source = records_by_turn.get(&binding.source_turn());
        return matches!(
            delivery,
            DeliveryRequest::NextSafePoint {
                expected_active_turn,
            } if expected_active_turn == binding.source_turn()
        ) && record.order.priority() == AcceptedInputQueuePriority::Ordinary
            && source.is_some_and(|source| {
                source.order.acceptance_position() < record.order.acceptance_position()
                    && !matches!(
                        source.state,
                        AcceptedInputTurnSchedulingRecordState::Queued
                            | AcceptedInputTurnSchedulingRecordState::Active { .. }
                    )
                    && source.origin_configuration == record.origin_configuration
            });
    }

    if !origin_configuration_matches_delivery(delivery, &record.origin_configuration) {
        return false;
    }

    match (delivery, record.order.priority()) {
        (DeliveryRequest::StartWhenNoActiveTurn { .. }, AcceptedInputQueuePriority::Ordinary) => {
            true
        }
        (
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn,
                ..
            },
            AcceptedInputQueuePriority::Ordinary,
        ) => historical_target_precedes_origin(expected_active_turn, record, records_by_turn),
        (
            DeliveryRequest::Interrupt {
                expected_active_turn,
                ..
            },
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor },
        ) => {
            expected_active_turn == predecessor
                && (historical_target_precedes_origin(
                    expected_active_turn,
                    record,
                    records_by_turn,
                ) || preceding_non_accepted_terminals.contains(&expected_active_turn))
        }
        (
            DeliveryRequest::StartWhenNoActiveTurn { .. }
            | DeliveryRequest::AfterCurrentTurn { .. },
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. },
        )
        | (
            DeliveryRequest::Interrupt { .. } | DeliveryRequest::NextSafePoint { .. },
            AcceptedInputQueuePriority::Ordinary,
        )
        | (
            DeliveryRequest::NextSafePoint { .. },
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. },
        ) => false,
    }
}

fn origin_configuration_matches_delivery(
    delivery: DeliveryRequest,
    origin_configuration: &OriginConfiguration,
) -> bool {
    let configuration = match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => configuration,
        DeliveryRequest::NextSafePoint { .. } => return false,
    };

    configuration.expected_session_defaults_version()
        == origin_configuration.session_defaults_version()
        && match configuration.model() {
            crate::ModelSelectionOverride::UseSessionDefault => true,
            crate::ModelSelectionOverride::ReplaceWith(requested) => {
                origin_configuration.requested().model() == requested
            }
        }
}

fn historical_target_precedes_origin(
    expected_active_turn: TurnId,
    origin: &AcceptedInputTurnSchedulingRecord,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
) -> bool {
    records_by_turn
        .get(&expected_active_turn)
        .is_some_and(|target| {
            target.order.acceptance_position() < origin.order.acceptance_position()
                && !matches!(target.state, AcceptedInputTurnSchedulingRecordState::Queued)
        })
}

pub(super) fn validate_record_correlations(
    session: SessionId,
    record: &AcceptedInputTurnSchedulingRecord,
) -> Result<(), AcceptedInputSchedulingReconstitutionFailure> {
    if record.stored_session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::TurnSessionMismatch { turn: record.turn },
        );
    }
    if record.accepted_input_session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptedInputSessionMismatch {
                turn: record.turn,
            },
        );
    }
    if record.queue_session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::QueueSessionMismatch {
                turn: record.turn,
            },
        );
    }
    if record.queue_turn != record.turn {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::QueueTurnMismatch { turn: record.turn },
        );
    }
    let accepted_input_matches = match &record.configuration_provenance {
        TurnConfigurationProvenance::ExplicitOrigin(configuration) => {
            configuration == &record.origin_configuration
                && record.accepted_input.disposition()
                    == &AcceptedInputDisposition::OriginOf(record.turn)
        }
        TurnConfigurationProvenance::InheritedForReclassifiedSteering(_) => matches!(
            record.accepted_input.disposition(),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn,
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            } if *turn == record.turn
        ),
    };
    if !accepted_input_matches {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptedInputOriginMismatch {
                turn: record.turn,
            },
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_start(
    index: usize,
    turn: TurnId,
    actual_lineage: AcceptedInputStartingLineage,
    starting_frontier: ContextFrontierId,
    initial_seed: Option<&ResolvedContextFrontierSnapshot>,
    previous_terminal: Option<&(TurnId, ResolvedContextFrontierSnapshot)>,
    origin_by_turn: &BTreeMap<TurnId, SemanticTranscriptEntryRef>,
    model_identity_entry: Option<SemanticTranscriptEntryRef>,
    compaction_chain: &[&crate::ContextCompaction],
    placement_frontiers: &[ContextFrontierId],
    snapshots: &BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    referenced_snapshots: &mut BTreeSet<ContextFrontierId>,
) -> Result<AcceptedInputTurnStart, AcceptedInputSchedulingReconstitutionFailure> {
    let expected_lineage = match (index, previous_terminal) {
        (0, None) => AcceptedInputStartingLineage::FirstInSession,
        (_, Some((predecessor, _))) => AcceptedInputStartingLineage::After {
            immediate_predecessor: *predecessor,
        },
        _ => {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn },
            );
        }
    };
    if actual_lineage != expected_lineage {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::StartingLineageMismatch {
                turn,
                expected: expected_lineage,
                actual: actual_lineage,
            },
        );
    }
    let snapshot = snapshots
        .get(&starting_frontier)
        .ok_or(AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn })?;
    if !referenced_snapshots.insert(starting_frontier) {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch { turn },
        );
    }
    let origin = origin_by_turn
        .get(&turn)
        .copied()
        .ok_or(AcceptedInputSchedulingReconstitutionFailure::MissingOriginEntry { turn })?;
    let prefix = previous_terminal
        .map(|(_, frontier)| frontier)
        .or_else(|| (index == 0).then_some(initial_seed).flatten());
    let mut suffix = Vec::with_capacity(usize::from(model_identity_entry.is_some()) + 1);
    suffix.extend(model_identity_entry);
    suffix.push(origin);
    let uncompacted_matches = prefix.map_or_else(
        || {
            snapshot.entry_count() == suffix.len()
                && snapshot.ordered_entries().eq(suffix.iter().copied())
        },
        |prefix| snapshot.has_semantic_prefix_and_suffix(prefix, suffix.iter().copied()),
    );
    let mut compaction_before_start = None;
    for compaction in compaction_chain {
        let Some(source) = snapshots.get(&compaction.source_frontier().snapshot()) else {
            break;
        };
        if snapshot.is_semantic_prefix_of(source) {
            break;
        }
        compaction_before_start = snapshots.get(&compaction.result_frontier().snapshot());
    }
    let applicable_compaction = prefix.and_then(|prefix| {
        compaction_before_start.filter(|result| !result.is_semantic_prefix_of(prefix))
    });
    let membership_matches = applicable_compaction.map_or(uncompacted_matches, |result| {
        prefix.is_some_and(|prefix| {
            prefix.is_semantic_prefix_of(result)
                && snapshot.has_semantic_prefix_and_suffix(result, suffix.iter().copied())
        })
    });
    let placement_matches = placement_frontiers
        .iter()
        .filter_map(|frontier| snapshots.get(frontier))
        .rfind(|boundary| boundary.is_semantic_prefix_of(snapshot))
        .is_some_and(|boundary| {
            prefix.is_none_or(|prefix| prefix.is_semantic_prefix_of(boundary))
                && snapshot.has_semantic_prefix_and_suffix(boundary, suffix.iter().copied())
        });
    if !membership_matches && !placement_matches {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch { turn },
        );
    }
    Ok(AcceptedInputTurnStart::from_validated_eligibility(
        actual_lineage,
        snapshot.frontier(),
    ))
}

pub(super) fn completed_terminal_matches(
    starting: &ResolvedContextFrontierSnapshot,
    terminal: &ResolvedContextFrontierSnapshot,
    completing_call: crate::ModelCallId,
    assistant_entries: &BTreeSet<SemanticTranscriptEntryRef>,
    completion_entry: SemanticTranscriptEntryRef,
    semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
) -> bool {
    let assistant_start = starting.entry_count();
    let assistant_end = terminal.entry_count().saturating_sub(1);
    if terminal.ordered_entries().next_back() != Some(completion_entry)
        || !starting.is_semantic_prefix_of(terminal)
        || assistant_end != assistant_start + assistant_entries.len()
    {
        return false;
    }

    terminal
        .ordered_entries_range(assistant_start, assistant_end)
        .all(|entry| {
            assistant_entries.contains(&entry)
                && matches!(
                    semantic_entries.get(&entry).map(SemanticTranscriptEntry::payload),
                    Some(
                        InitialSemanticTranscriptEntryPayload::AssistantText {
                            producing_call,
                            ..
                        }
                        | InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                            producing_call,
                            ..
                        }
                    ) if *producing_call == completing_call
                )
        })
}

pub(super) fn refused_terminal_matches(
    source: &ResolvedContextFrontierSnapshot,
    terminal: &ResolvedContextFrontierSnapshot,
    refusing_call: crate::ModelCallId,
    assistant_entries: &BTreeSet<SemanticTranscriptEntryRef>,
    semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
) -> bool {
    let suffix_start = source.entry_count();
    if !source.is_semantic_prefix_of(terminal)
        || terminal.entry_count() != suffix_start + assistant_entries.len()
    {
        return false;
    }
    terminal
        .ordered_entries_range(suffix_start, terminal.entry_count())
        .all(|entry| {
            assistant_entries.contains(&entry)
                && matches!(
                    semantic_entries.get(&entry).map(SemanticTranscriptEntry::payload),
                    Some(InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call,
                        ..
                    }) if *producing_call == refusing_call
                )
        })
}

/// Returns the one completed call whose proposals and correlated result suffix
/// fill `terminal` up to its ending `terminal_marker`, or `None` when no call
/// or more than one call can claim that suffix.
///
/// Terminal materialization closes every request that did not complete
/// ordinary execution as `ToolClosed`, so this window admits closed stand-ins.
#[allow(clippy::too_many_arguments)]
pub(super) fn tool_round_terminal_producing_call(
    turn: TurnId,
    terminal: &ResolvedContextFrontierSnapshot,
    terminal_marker: SemanticTranscriptEntryRef,
    terminal_tool_attempts: &[crate::EndedToolAttempt],
    terminal_tool_denials: &[ToolApprovalResolution],
    model_calls: &BTreeMap<crate::ModelCallId, ReconstitutedModelCall>,
    assistant_by_call: &BTreeMap<crate::ModelCallId, BTreeSet<SemanticTranscriptEntryRef>>,
    snapshots: &BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
) -> Option<crate::ModelCallId> {
    if terminal.ordered_entries().next_back() != Some(terminal_marker) {
        return None;
    }
    tool_round_producing_call_in_window(
        turn,
        terminal,
        terminal.entry_count().saturating_sub(1),
        ToolRoundResultWindow::TerminalClosure,
        terminal_tool_attempts,
        terminal_tool_denials,
        model_calls,
        assistant_by_call,
        snapshots,
        semantic_entries,
    )
}

/// Returns the one completed call whose proposals and correlated result suffix
/// fill `snapshot` up to `results_end`, or `None` when no call or more than
/// one call can claim that window.
///
/// Continuation happens only once every request is executed or denied, so this
/// window forbids `ToolClosed` stand-ins and admits only the attempt ends the
/// continuation writer projects.
#[allow(clippy::too_many_arguments)]
pub(super) fn tool_round_continuation_producing_call(
    turn: TurnId,
    snapshot: &ResolvedContextFrontierSnapshot,
    results_end: usize,
    round_tool_attempts: &[crate::EndedToolAttempt],
    round_tool_denials: &[ToolApprovalResolution],
    model_calls: &BTreeMap<crate::ModelCallId, ReconstitutedModelCall>,
    assistant_by_call: &BTreeMap<crate::ModelCallId, BTreeSet<SemanticTranscriptEntryRef>>,
    snapshots: &BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
) -> Option<crate::ModelCallId> {
    tool_round_producing_call_in_window(
        turn,
        snapshot,
        results_end,
        ToolRoundResultWindow::Continuation,
        round_tool_attempts,
        round_tool_denials,
        model_calls,
        assistant_by_call,
        snapshots,
        semantic_entries,
    )
}

/// Which materialization owns a checked tool-round result window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolRoundResultWindow {
    /// Turn-end materialization: requests that did not complete ordinary
    /// execution close as `ToolClosed`, and a crash-lost known-failed attempt
    /// projects its result directly.
    TerminalClosure,
    /// The continuation transaction: every request executed or denied, so
    /// closure stand-ins and non-projectable attempt ends are forbidden.
    Continuation,
}

/// Returns the one completed call whose proposals and correlated result
/// entries fill `snapshot`'s window ending at `results_end`, or `None` when no
/// call or more than one call can claim it.
#[allow(clippy::too_many_arguments)]
fn tool_round_producing_call_in_window(
    turn: TurnId,
    terminal: &ResolvedContextFrontierSnapshot,
    before_marker_end: usize,
    window: ToolRoundResultWindow,
    terminal_tool_attempts: &[crate::EndedToolAttempt],
    terminal_tool_denials: &[ToolApprovalResolution],
    model_calls: &BTreeMap<crate::ModelCallId, ReconstitutedModelCall>,
    assistant_by_call: &BTreeMap<crate::ModelCallId, BTreeSet<SemanticTranscriptEntryRef>>,
    snapshots: &BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
) -> Option<crate::ModelCallId> {
    // A continuation window projects only executed results the writer admits:
    // a completed attempt or an ordinary known failure. An ambiguous or
    // crash-lost end is a turn-level failure that can never reach a
    // continuation, while terminal materialization projects the crash-lost
    // known-failed attempt directly.
    if window == ToolRoundResultWindow::Continuation
        && terminal_tool_attempts
            .iter()
            .any(|attempt| match attempt.end() {
                crate::ToolAttemptEnd::Completed { .. } => false,
                crate::ToolAttemptEnd::KnownFailed { error } => {
                    error.kind() == crate::ToolExecutionErrorKind::CrashLost
                }
                crate::ToolAttemptEnd::AwaitingChild { .. } => false,
                crate::ToolAttemptEnd::Ambiguous => true,
            })
    {
        return None;
    }
    let mut denied_requests = BTreeSet::new();
    for resolution in terminal_tool_denials {
        if !matches!(resolution.decision(), ToolApprovalDecision::Deny { .. })
            || !denied_requests.insert(resolution.request())
        {
            return None;
        }
    }

    let mut producing_calls = model_calls
        .iter()
        .filter(|(call_id, candidate)| {
            let ReconstitutedModelCall::Ended(call) = candidate else {
                return false;
            };
            if call.turn() != turn || call.disposition() != ModelCallDisposition::Completed {
                return false;
            }
            let Some(source) = snapshots.get(&call.frontier().snapshot()) else {
                return false;
            };
            if !source.is_semantic_prefix_of(terminal) {
                return false;
            }
            let Some(assistant_entries) = assistant_by_call.get(call_id) else {
                return false;
            };
            let assistant_start = source.entry_count();
            let assistant_end = assistant_start + assistant_entries.len();
            if assistant_end > before_marker_end {
                return false;
            }

            let mut requests = Vec::new();
            for entry in terminal.ordered_entries_range(assistant_start, assistant_end) {
                if !assistant_entries.contains(&entry) {
                    return false;
                }
                match semantic_entries
                    .get(&entry)
                    .map(SemanticTranscriptEntry::payload)
                {
                    Some(SemanticTranscriptEntryPayload::AssistantText {
                        producing_call, ..
                    }) if producing_call == *call_id => {}
                    Some(SemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call,
                        ..
                    }) if producing_call == *call_id => {}
                    Some(SemanticTranscriptEntryPayload::AssistantToolUse {
                        producing_call,
                        request,
                    }) if producing_call == *call_id => requests.push(*request),
                    _ => return false,
                }
            }
            if requests.is_empty()
                || requests.iter().copied().collect::<BTreeSet<_>>().len() != requests.len()
                || before_marker_end - assistant_end != requests.len()
            {
                return false;
            }

            let mut attempts_by_id = BTreeMap::new();
            for attempt in terminal_tool_attempts {
                if attempt.session() != terminal.frontier().owning_session()
                    || attempt.turn() != turn
                    || attempts_by_id.insert(attempt.attempt(), attempt).is_some()
                {
                    return false;
                }
            }
            let mut observed_attempts = BTreeSet::new();
            let mut observed_denials = BTreeSet::new();
            let results_match = terminal
                .ordered_entries_range(assistant_end, before_marker_end)
                .zip(requests)
                .all(|(entry, request)| {
                    match semantic_entries
                        .get(&entry)
                        .map(SemanticTranscriptEntry::payload)
                    {
                        Some(SemanticTranscriptEntryPayload::ToolExecutionResult { attempt }) => {
                            observed_attempts.insert(*attempt)
                                && attempts_by_id
                                    .get(attempt)
                                    .is_some_and(|ended| ended.request() == request)
                        }
                        Some(SemanticTranscriptEntryPayload::ToolDenied { request: actual }) => {
                            *actual == request
                                && denied_requests.contains(actual)
                                && observed_denials.insert(*actual)
                        }
                        Some(SemanticTranscriptEntryPayload::ToolClosed { request: actual }) => {
                            window == ToolRoundResultWindow::TerminalClosure && *actual == request
                        }
                        _ => false,
                    }
                });
            results_match
                && observed_attempts.len() == attempts_by_id.len()
                && observed_denials.len() == denied_requests.len()
        })
        .map(|(call_id, _)| *call_id);
    let producing_call = producing_calls.next()?;
    producing_calls.next().is_none().then_some(producing_call)
}
