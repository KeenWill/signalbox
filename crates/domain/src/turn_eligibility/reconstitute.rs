//! Turn scheduling reconstitute for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::acceptance_tail::{
    ActiveAcceptanceTailReconstitutionEvidence, reconstitute_active_acceptance_tail,
};
use super::correlation::{
    completed_terminal_matches, origin_delivery_matches_record, refused_terminal_matches,
    tool_reconciliation_attempt_end_matches, tool_round_continuation_producing_call,
    tool_round_terminal_producing_call, validate_record_correlations, validate_start,
};
use super::failure::{
    AcceptedInputSchedulingReconstitutionError, AcceptedInputSchedulingReconstitutionFailure,
};
use super::projection::{
    AcceptedInputSchedulingProjection, AcceptedInputTurnSchedulingProjection,
    ActiveExecutingToolBatchCorrelation, ActiveModelCallRecoveryWait, ReconstitutedSchedulingState,
};
use super::reconstitution_input::{
    AcceptedInputSchedulingReconstitutionInput, StoredActiveTurnPhase,
    TerminalAttemptEndReconstitutionInput,
};
use super::record::{
    AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState,
    AutomaticReconciliationAuthority, DelegatedTurnSchedulingState,
};
use crate::context_frontier::ContextFrontierEntryValidationCache;
use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputQueueOrder, AcceptedInputQueuePriority,
    AcceptedInputQueueWork, ActiveTurnPhase, AppliedInterruptCommandResult, AttemptEnd,
    CancellationStopDisposition, CurrentTurnAttempt, DeliveryRequest,
    InitialSemanticTranscriptEntryPayload, ModelCallDisposition, NonEmptyIssuedOperationRefs,
    ReconstitutedModelCall, ResolvedContextFrontierSnapshot, SemanticTranscriptEntry,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef, SessionId, SessionInputPosition,
    TranscriptAncestry, TurnId, UnstoppedAttemptDisposition, derive_accepted_input_total_order,
};
use std::{collections::BTreeMap, collections::BTreeSet};

pub(super) fn reconstitute(
    input: AcceptedInputSchedulingReconstitutionInput,
) -> Result<AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionError> {
    match reconstitute_inner(&input) {
        Ok(projection) => Ok(projection),
        Err(failure) => Err(AcceptedInputSchedulingReconstitutionError {
            input: Box::new(input),
            failure,
        }),
    }
}

fn applied_interrupt_matches_scheduling(
    interrupt: AppliedInterruptCommandResult,
    session: SessionId,
    predecessor: TurnId,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
) -> bool {
    let successor = records_by_turn.get(&interrupt.successor());
    interrupt.session() == session
        && interrupt.proof().predecessor() == predecessor
        && successor.is_some_and(|successor| {
            successor.stored_session == session
                && successor.accepted_input.id() == interrupt.accepted_input()
                && successor.order == interrupt.successor_order()
        })
}

fn terminal_attempt_end_matches(
    attempt_end: &TerminalAttemptEndReconstitutionInput,
    session: SessionId,
    turn: TurnId,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
    allowed_without_stop: &[UnstoppedAttemptDisposition],
    allowed_after_cancellation: &[CancellationStopDisposition],
) -> bool {
    match attempt_end.end() {
        AttemptEnd::WithoutStop { disposition } => {
            attempt_end.interrupt().is_none() && allowed_without_stop.contains(disposition)
        }
        AttemptEnd::AfterCancellation { cause, disposition } => {
            allowed_after_cancellation.contains(disposition)
                && attempt_end.interrupt().is_some_and(|interrupt| {
                    interrupt.proof() == *cause
                        && applied_interrupt_matches_scheduling(
                            interrupt,
                            session,
                            turn,
                            records_by_turn,
                        )
                })
        }
        AttemptEnd::AfterFatalMismatch { .. } => false,
    }
}

fn reconstitute_inner(
    input: &AcceptedInputSchedulingReconstitutionInput,
) -> Result<AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionFailure> {
    let (imported_session, bounded_imported_seed) = match (
        input.session.creation_provenance().ancestry(),
        input.imported_session.as_ref(),
        input.bounded_imported_seed.as_ref(),
    ) {
        (TranscriptAncestry::None, None, None) => (None, None),
        (TranscriptAncestry::None, Some(_), _) | (TranscriptAncestry::None, _, Some(_)) => {
            return Err(AcceptedInputSchedulingReconstitutionFailure::UnexpectedImportedSession);
        }
        (TranscriptAncestry::ImportedConversation { .. }, None, None) => {
            return Err(AcceptedInputSchedulingReconstitutionFailure::MissingImportedSession);
        }
        (TranscriptAncestry::ImportedConversation { .. }, Some(_), Some(_)) => {
            return Err(AcceptedInputSchedulingReconstitutionFailure::ImportedSessionMismatch);
        }
        (TranscriptAncestry::ImportedConversation { .. }, Some(imported), None)
            if imported.session() == &input.session =>
        {
            (Some(imported), None)
        }
        (TranscriptAncestry::ImportedConversation { .. }, Some(_), None) => {
            return Err(AcceptedInputSchedulingReconstitutionFailure::ImportedSessionMismatch);
        }
        (
            TranscriptAncestry::ImportedConversation {
                source_frontier, ..
            },
            None,
            Some(seed),
        ) if seed.owning_session() == input.session.id()
            && seed.declared_member_count() == source_frontier.through_position().as_u64() =>
        {
            (None, Some(seed))
        }
        (TranscriptAncestry::ImportedConversation { .. }, None, Some(_)) => {
            return Err(AcceptedInputSchedulingReconstitutionFailure::ImportedSessionMismatch);
        }
        (TranscriptAncestry::SingleSource { .. }, _, _) => {
            return Err(AcceptedInputSchedulingReconstitutionFailure::UnsupportedSessionAncestry);
        }
    };

    let session = input.session.id();
    let mut accepted_input_turns = BTreeMap::new();
    for record in &input.turns {
        validate_record_correlations(session, record)?;
        if accepted_input_turns
            .insert(record.accepted_input.id(), record.turn)
            .is_some()
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptedInput {
                    accepted_input: record.accepted_input.id(),
                },
            );
        }
    }

    let records_by_turn = input
        .turns
        .iter()
        .map(|record| (record.turn, record))
        .collect::<BTreeMap<_, _>>();
    let mut preceding_non_accepted_terminal_turns = BTreeSet::new();
    let mut preceding_non_accepted_successors = BTreeMap::new();
    for (stored_session, predecessor, successor, _, _) in &input.preceding_non_accepted_terminals {
        if *stored_session != session
            || records_by_turn.contains_key(predecessor)
            || !records_by_turn.contains_key(successor)
            || !preceding_non_accepted_terminal_turns.insert(*predecessor)
            || preceding_non_accepted_successors
                .insert(*successor, *predecessor)
                .is_some()
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::TurnSessionMismatch {
                    turn: *predecessor,
                },
            );
        }
    }
    let mut external_interrupt_successors = BTreeSet::new();
    for (successor, predecessor) in &preceding_non_accepted_successors {
        match records_by_turn[successor].order.priority() {
            AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: recorded,
            } if recorded == *predecessor => {
                external_interrupt_successors.insert(*successor);
            }
            AcceptedInputQueuePriority::Ordinary => {}
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. } => {
                return Err(
                    AcceptedInputSchedulingReconstitutionFailure::TurnSessionMismatch {
                        turn: *predecessor,
                    },
                );
            }
        }
    }
    // The generic accepted-input order owns only accepted origins. A
    // runtime-relevant record can instead be the immediate successor of a
    // separately authenticated non-accepted terminal boundary; treat every
    // such external edge as a derivation root while retaining and validating
    // interrupt priority below.
    let ordinary_roots = input
        .turns
        .iter()
        .filter(|record| record.order.priority() == AcceptedInputQueuePriority::Ordinary)
        .map(|record| record.turn)
        .collect::<BTreeSet<_>>();
    let queued_turns = input
        .turns
        .iter()
        .filter(|record| matches!(record.state, AcceptedInputTurnSchedulingRecordState::Queued))
        .map(|record| record.turn)
        .collect::<BTreeSet<_>>();
    let queue_work = input.turns.iter().map(|record| {
        let order = if preceding_non_accepted_successors.contains_key(&record.turn) {
            AcceptedInputQueueOrder::ordinary(record.order.acceptance_position())
        } else {
            record.order
        };
        AcceptedInputQueueWork::new(record.queue_session, record.queue_turn, order)
    });
    let total_order = promote_external_interrupt_chains(
        derive_accepted_input_total_order(queue_work).map_err(|error| {
            AcceptedInputSchedulingReconstitutionFailure::InvalidQueueOrder { error }
        })?,
        external_interrupt_successors,
        &ordinary_roots,
        &queued_turns,
    );
    let execution_position_by_turn = total_order
        .iter()
        .copied()
        .enumerate()
        .map(|(position, turn)| (turn, position))
        .collect::<BTreeMap<_, _>>();
    let mut delegated_turns = BTreeMap::new();
    for fact in input.delegated_turns.iter().copied() {
        let turn = fact.turn();
        if records_by_turn.contains_key(&turn) || delegated_turns.insert(turn, fact).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DelegatedTurnFactMismatch { turn },
            );
        }
    }
    for record in records_by_turn.values() {
        if !origin_delivery_matches_record(
            record.origin_delivery,
            record,
            &records_by_turn,
            &preceding_non_accepted_terminal_turns,
        ) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                    turn: record.turn,
                },
            );
        }
    }

    let mut semantic_entries = imported_session
        .into_iter()
        .flat_map(|imported| imported.semantic_entries())
        .map(|entry| (entry.reference(), entry.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut origin_by_turn = BTreeMap::new();
    let mut failure_by_turn = BTreeMap::new();
    let mut steering_by_input = BTreeMap::new();
    let mut model_identity_by_turn = BTreeMap::new();
    let mut summary_by_call = BTreeMap::new();
    let mut assistant_by_call = BTreeMap::<crate::ModelCallId, BTreeSet<_>>::new();
    let mut completion_by_turn = BTreeMap::new();
    let mut cancellation_by_turn = BTreeMap::new();
    for candidate in &input.semantic_entries {
        if candidate.source_session() != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySourceSessionMismatch {
                    entry: candidate.identity(),
                },
            );
        }

        let entry = SemanticTranscriptEntry::from_validated_parts(
            candidate.identity(),
            candidate.source_session(),
            candidate.payload().clone(),
        );
        let entry_reference = entry.reference();
        if semantic_entries.insert(entry_reference, entry).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntry {
                    entry: entry_reference,
                },
            );
        }

        match candidate.payload() {
            InitialSemanticTranscriptEntryPayload::Imported { .. } => {
                return Err(
                    AcceptedInputSchedulingReconstitutionFailure::UnsupportedSemanticEntry {
                        entry: candidate.identity(),
                    },
                );
            }
            InitialSemanticTranscriptEntryPayload::ContextSummary { producing_call, .. } => {
                if summary_by_call
                    .insert(*producing_call, entry_reference)
                    .is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input } => {
                let Some(turn) = accepted_input_turns.get(accepted_input).copied() else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                let record = records_by_turn[&turn];
                if matches!(
                    &record.state,
                    AcceptedInputTurnSchedulingRecordState::Queued
                        | AcceptedInputTurnSchedulingRecordState::Retired
                ) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if origin_by_turn.insert(turn, entry_reference).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input,
                source_turn,
            } => {
                if steering_by_input
                    .insert(*accepted_input, (entry_reference, *source_turn))
                    .is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                turn,
                defaults_version,
                selected,
            } => {
                let Some(configuration_matches) = records_by_turn
                    .get(turn)
                    .map(|record| {
                        !matches!(
                            &record.state,
                            AcceptedInputTurnSchedulingRecordState::Queued
                        ) && record.origin_configuration.session_defaults_version()
                            == *defaults_version
                            && record
                                .origin_configuration
                                .effective()
                                .model()
                                .selected_direct()
                                == *selected
                    })
                    .or_else(|| {
                        delegated_turns.get(turn).map(|fact| {
                            fact.defaults_version() == *defaults_version
                                && fact.selected() == *selected
                        })
                    })
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                if !configuration_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if model_identity_by_turn
                    .insert(*turn, entry_reference)
                    .is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::TurnFailed { turn } => {
                let Some(state_matches) = records_by_turn
                    .get(turn)
                    .map(|record| {
                        matches!(
                            &record.state,
                            AcceptedInputTurnSchedulingRecordState::TerminalFailed { .. }
                        )
                    })
                    .or_else(|| {
                        delegated_turns.get(turn).map(|fact| {
                            fact.state() == DelegatedTurnSchedulingState::TerminalFailed
                        })
                    })
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                if !state_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if failure_by_turn.insert(*turn, entry_reference).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::AssistantText { producing_call, .. }
            | InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call, ..
            }
            | InitialSemanticTranscriptEntryPayload::ProviderReasoning { producing_call, .. }
            | InitialSemanticTranscriptEntryPayload::AssistantToolUse { producing_call, .. } => {
                assistant_by_call
                    .entry(*producing_call)
                    .or_default()
                    .insert(entry_reference);
            }
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | InitialSemanticTranscriptEntryPayload::ToolDenied { .. }
            | InitialSemanticTranscriptEntryPayload::ToolInadmissible { .. }
            | InitialSemanticTranscriptEntryPayload::ToolClosed { .. }
            | InitialSemanticTranscriptEntryPayload::RunnerPlacementChanged { .. } => {}
            InitialSemanticTranscriptEntryPayload::DelegatedTask { .. }
            | InitialSemanticTranscriptEntryPayload::DelegationMessage { .. }
            | InitialSemanticTranscriptEntryPayload::DelegationResult { .. } => {}
            InitialSemanticTranscriptEntryPayload::TurnCompleted { turn } => {
                let Some(state_matches) = records_by_turn
                    .get(turn)
                    .map(|record| {
                        matches!(
                            &record.state,
                            AcceptedInputTurnSchedulingRecordState::TerminalCompleted { .. }
                        )
                    })
                    .or_else(|| {
                        delegated_turns.get(turn).map(|fact| {
                            fact.state() == DelegatedTurnSchedulingState::TerminalCompleted
                        })
                    })
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                if !state_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if completion_by_turn.insert(*turn, entry_reference).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::TurnCancelled { turn } => {
                let Some(state_matches) = records_by_turn
                    .get(turn)
                    .map(|record| {
                        matches!(
                            &record.state,
                            AcceptedInputTurnSchedulingRecordState::TerminalCancelled { .. }
                        )
                    })
                    .or_else(|| {
                        delegated_turns.get(turn).map(|fact| {
                            fact.state() == DelegatedTurnSchedulingState::TerminalCancelled
                        })
                    })
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                if !state_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if cancellation_by_turn
                    .insert(*turn, entry_reference)
                    .is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
        }
    }

    let initial_seed_frontier = imported_session
        .map(|imported| imported.seed_snapshot().frontier().snapshot())
        .or_else(|| bounded_imported_seed.map(|seed| seed.seed_frontier()));
    let mut snapshots = imported_session
        .into_iter()
        .map(|imported| {
            (
                imported.seed_snapshot().frontier().snapshot(),
                imported.seed_snapshot().clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut frontier_entry_validation = ContextFrontierEntryValidationCache::default();
    for candidate in &input.snapshots {
        if candidate.owning_session() != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SnapshotOwningSessionMismatch {
                    snapshot: candidate.snapshot(),
                },
            );
        }
        if let Some(entry) = candidate
            .first_missing_entry(&mut frontier_entry_validation, |entry| {
                semantic_entries.contains_key(entry)
            })
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SnapshotEntryMissing {
                    snapshot: candidate.snapshot(),
                    entry,
                },
            );
        }
        let snapshot = candidate.snapshot();
        let resolved =
            ResolvedContextFrontierSnapshot::try_from_reconstitution_input(candidate.clone())
                .map_err(|_| {
                    AcceptedInputSchedulingReconstitutionFailure::InvalidSnapshotMembership {
                        snapshot,
                    }
                })?;
        if snapshots.insert(snapshot, resolved).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateSnapshot { snapshot },
            );
        }
    }

    let mut preceding_non_accepted_terminals = BTreeMap::new();
    for (_, turn, _, terminal_frontier, selected) in &input.preceding_non_accepted_terminals {
        let snapshot = snapshots.get(terminal_frontier).cloned().ok_or(
            AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn: *turn },
        )?;
        preceding_non_accepted_terminals.insert(*turn, (snapshot, *selected));
    }

    let mut compaction_calls = BTreeMap::new();
    let mut compaction_snapshots = BTreeSet::new();
    for candidate in &input.compaction_calls {
        let call = candidate.id();
        let source = snapshots.get(&candidate.source_snapshot()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionCallSnapshotMissing { call },
        )?;
        let reconstituted = candidate.clone().reconstitute(source).map_err(|_| {
            AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionCall { call }
        })?;
        compaction_snapshots.insert(candidate.source_snapshot());
        if compaction_calls.insert(call, reconstituted).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateCompactionCall { call },
            );
        }
    }
    let mut compactions = BTreeMap::new();
    let mut referenced_compaction_calls = BTreeSet::new();
    for candidate in &input.compactions {
        let compaction = candidate.id();
        let source = snapshots.get(&candidate.source_snapshot()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionSnapshotMissing { compaction },
        )?;
        let result = snapshots.get(&candidate.result_snapshot()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionSnapshotMissing { compaction },
        )?;
        compaction_snapshots.insert(candidate.source_snapshot());
        compaction_snapshots.insert(candidate.result_snapshot());
        let call = compaction_calls.get(&candidate.producing_call()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionEvidenceMissing { compaction },
        )?;
        let summary_reference = summary_by_call.get(&candidate.producing_call()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionEvidenceMissing { compaction },
        )?;
        if summary_reference.entry() != candidate.summary_entry() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::CompactionEvidenceMissing {
                    compaction,
                },
            );
        }
        let summary = semantic_entries.get(summary_reference).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionEvidenceMissing { compaction },
        )?;
        let source_entries = source
            .ordered_entries()
            .map(|reference| semantic_entries[&reference].clone())
            .collect::<Vec<_>>();
        let result_entries = result
            .ordered_entries()
            .map(|reference| semantic_entries[&reference].clone())
            .collect::<Vec<_>>();
        let reconstituted = candidate
            .clone()
            .reconstitute(
                source,
                result,
                &source_entries,
                &result_entries,
                summary,
                call,
            )
            .map_err(
                |_| AcceptedInputSchedulingReconstitutionFailure::InvalidCompaction { compaction },
            )?;
        referenced_compaction_calls.insert(candidate.producing_call());
        if compactions.insert(compaction, reconstituted).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateCompaction { compaction },
            );
        }
    }
    let mut root = None;
    let mut predecessors = BTreeSet::new();
    for compaction in compactions.values() {
        let Some(predecessor) = compaction.predecessor() else {
            if root.replace(compaction.id()).is_some() {
                return Err(
                    AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain {
                        compaction: compaction.id(),
                    },
                );
            }
            continue;
        };
        if !predecessors.insert(predecessor) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain {
                    compaction: compaction.id(),
                },
            );
        }
        let previous = compactions.get(&predecessor).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain {
                compaction: compaction.id(),
            },
        )?;
        let previous_result = snapshots[&previous.result_frontier().snapshot()].clone();
        let source = &snapshots[&compaction.source_frontier().snapshot()];
        if !previous_result.is_semantic_prefix_of(source) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain {
                    compaction: compaction.id(),
                },
            );
        }
    }
    if root.is_none()
        && let Some(compaction) = compactions.keys().next().copied()
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain { compaction },
        );
    }
    let mut leaves = compactions
        .values()
        .filter(|compaction| !predecessors.contains(&compaction.id()));
    let latest_compaction = leaves.next();
    if let Some(extra) = leaves.next() {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain {
                compaction: extra.id(),
            },
        );
    }
    let mut chain_members = BTreeSet::new();
    let mut chain_ids = Vec::with_capacity(compactions.len());
    let mut chain_cursor = latest_compaction.map(crate::ContextCompaction::id);
    while let Some(compaction) = chain_cursor {
        if !chain_members.insert(compaction) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain { compaction },
            );
        }
        chain_ids.push(compaction);
        chain_cursor = compactions[&compaction].predecessor();
    }
    chain_ids.reverse();
    let compaction_chain = chain_ids
        .iter()
        .map(|compaction| &compactions[compaction])
        .collect::<Vec<_>>();
    if let Some(compaction) = compactions
        .keys()
        .find(|compaction| !chain_members.contains(compaction))
        .copied()
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain { compaction },
        );
    }
    let latest_compaction_result =
        latest_compaction.map(|compaction| compaction.result_frontier().snapshot());
    if let Some(call) = summary_by_call
        .keys()
        .find(|call| !referenced_compaction_calls.contains(call))
        .copied()
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedCompactionEvidence { call },
        );
    }
    if let Some(call) = compaction_calls
        .values()
        .find(|call| {
            call.state()
                == crate::ContextCompactionModelCallState::Terminal(ModelCallDisposition::Completed)
                && !referenced_compaction_calls.contains(&call.id())
        })
        .map(crate::ContextCompactionModelCall::id)
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedCompactionEvidence { call },
        );
    }
    let mut active_compaction_calls = compaction_calls.values().filter(|call| {
        matches!(
            call.state(),
            crate::ContextCompactionModelCallState::Prepared
                | crate::ContextCompactionModelCallState::InFlight
        )
    });
    let active_compaction_call = active_compaction_calls
        .next()
        .map(crate::ContextCompactionModelCall::id);
    if let Some(call) = active_compaction_calls
        .next()
        .map(crate::ContextCompactionModelCall::id)
    {
        return Err(AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionCall { call });
    }

    let mut pinned_targets = BTreeMap::new();
    for candidate in &input.pinned_targets {
        let turn = candidate.turn();
        let Some(pinned) = candidate.reconstitute_for_turn(turn) else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::UnreferencedPinnedTarget { turn },
            );
        };
        if pinned_targets.insert(turn, pinned).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicatePinnedTarget { turn },
            );
        }
    }
    let mut referenced_pinned_targets = BTreeSet::new();
    let mut model_calls = BTreeMap::new();
    for candidate in &input.model_calls {
        let call = candidate.id();
        if compaction_calls.contains_key(&call) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateModelCallIdentityAcrossKinds {
                    call,
                },
            );
        }
        let snapshot = snapshots.get(&candidate.frontier()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::ModelCallSnapshotMissing { call },
        )?;
        let Some(pinned) = pinned_targets.get(&candidate.turn()).copied() else {
            return Err(AcceptedInputSchedulingReconstitutionFailure::PinnedTargetMissing { call });
        };
        let reconstituted = candidate
            .reconstitute(snapshot, pinned)
            .map_err(|_| AcceptedInputSchedulingReconstitutionFailure::InvalidModelCall { call })?;
        referenced_pinned_targets.insert(candidate.turn());
        if model_calls.insert(call, reconstituted).is_some() {
            return Err(AcceptedInputSchedulingReconstitutionFailure::DuplicateModelCall { call });
        }
    }
    let model_call_inputs = input
        .model_calls
        .iter()
        .map(|call| (call.id(), call))
        .collect::<BTreeMap<_, _>>();
    let mut steering_round_evidence = BTreeMap::new();
    for round in &input.steering_continuation_rounds {
        if steering_round_evidence
            .insert(round.call(), round)
            .is_some()
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SteeringContinuationRoundMismatch {
                    call: round.call(),
                },
            );
        }
    }
    let mut continuation_round_evidence = BTreeMap::new();
    for round in &input.continuation_rounds {
        if continuation_round_evidence
            .insert(round.call(), round)
            .is_some()
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ContinuationRoundMismatch {
                    call: round.call(),
                },
            );
        }
    }
    let mut consumed_inputs = BTreeMap::new();
    let mut consumed_by_call = BTreeMap::<
        crate::ModelCallId,
        Vec<(
            SessionInputPosition,
            SemanticTranscriptEntryRef,
            AcceptedInputId,
        )>,
    >::new();
    for consumed in &input.consumed_steering {
        let accepted_input = consumed.accepted_input.id();
        if consumed.session != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringSessionMismatch {
                    accepted_input,
                },
            );
        }
        if consumed_inputs.contains_key(&accepted_input) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateConsumedSteering {
                    accepted_input,
                },
            );
        }
        let Some((entry, semantic_source_turn)) = steering_by_input.remove(&accepted_input) else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        };
        let AcceptedInputDisposition::ConsumedAsSteering { call } =
            consumed.accepted_input.disposition()
        else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        };
        consumed_inputs.insert(accepted_input, *call);
        let source_record = records_by_turn.get(&consumed.source_turn).copied();
        let source_record_matches = source_record.is_some_and(|record| {
            !matches!(
                record.state,
                AcceptedInputTurnSchedulingRecordState::Queued
                    | AcceptedInputTurnSchedulingRecordState::Retired
            ) && record.order.acceptance_position() < consumed.acceptance_position
        });
        let active_tail_matches = input.active_acceptance_tail.as_ref().is_some_and(|tail| {
            tail.entries.iter().any(|candidate| {
                candidate.session == session
                    && candidate.accepted_input == consumed.accepted_input
                    && candidate.position == consumed.acceptance_position
                    && matches!(
                        candidate.delivery,
                        DeliveryRequest::NextSafePoint {
                            expected_active_turn,
                        } if expected_active_turn == consumed.source_turn
                            && expected_active_turn == semantic_source_turn
                    )
            })
        });
        let source_is_active = source_record.is_some_and(|record| {
            matches!(
                record.state,
                AcceptedInputTurnSchedulingRecordState::Active { .. }
            )
        });
        let earlier_reclassified = records_by_turn.values().any(|record| {
            record.order.acceptance_position() < consumed.acceptance_position
                && matches!(
                    record.accepted_input.disposition(),
                    AcceptedInputDisposition::ReclassifiedAsTurnOrigin { .. }
                )
                && matches!(
                    record.origin_delivery,
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn,
                    } if expected_active_turn == consumed.source_turn
                )
        });
        let model_call_matches =
            model_call_inputs
                .get(call)
                .zip(source_record)
                .is_some_and(|(model_call, record)| {
                    // A steering-consuming call that completed by proposing a
                    // tool round becomes model-visible history while its turn
                    // continues: later safe points, waits, and every terminal
                    // shape keep the consumed rows of earlier rounds. Such a
                    // consumer is correlated through its assistant entries
                    // (validated by the assistant-content law) and its exact
                    // frontier window (validated below), not through the
                    // current phase or the turn's terminal call. Only a tool
                    // proposal keeps a completed call's turn going, so a
                    // text-only completed consumer stays bound to the
                    // terminal correlation below.
                    let completed_history_consumer = !matches!(
                        &record.state,
                        AcceptedInputTurnSchedulingRecordState::Queued
                    ) && model_call.state()
                        == crate::ModelCallReconstitutionState::Terminal(
                            ModelCallDisposition::Completed,
                        )
                        && assistant_by_call
                            .get(&model_call.id())
                            .is_some_and(|entries| {
                                entries.iter().any(|entry| {
                                    matches!(
                                        semantic_entries
                                            .get(entry)
                                            .map(SemanticTranscriptEntry::payload),
                                        Some(
                                            SemanticTranscriptEntryPayload::AssistantToolUse { .. }
                                        )
                                    )
                                })
                            });
                    let lifecycle_matches = completed_history_consumer
                        || match &record.state {
                    AcceptedInputTurnSchedulingRecordState::Queued | AcceptedInputTurnSchedulingRecordState::Retired => false,
                    AcceptedInputTurnSchedulingRecordState::Active { phase, .. } => {
                        Some(model_call.attempt()) == phase.current_attempt
                            && match (&phase.state, model_call.state()) {
                                (
                                    StoredActiveTurnPhase::Prepared,
                                    crate::ModelCallReconstitutionState::Prepared,
                                )
                                | (
                                    StoredActiveTurnPhase::Running,
                                    crate::ModelCallReconstitutionState::InFlight,
                                ) => true,
                                // A continuation attempt that authorized
                                // physical tool execution is already Running
                                // when it receives its Prepared call, so this
                                // pair is legal exactly at a tool-round
                                // continuation boundary, proven by the
                                // round's result evidence and the frontier
                                // law below.
                                (
                                    StoredActiveTurnPhase::Running,
                                    crate::ModelCallReconstitutionState::Prepared,
                                ) => steering_round_evidence.contains_key(&model_call.id()),
                                (
                                    StoredActiveTurnPhase::StopRequested { call, .. },
                                    crate::ModelCallReconstitutionState::CancellationRequested,
                                ) => *call == model_call.id(),
                                (
                                    StoredActiveTurnPhase::AwaitingApproval { .. },
                                    _,
                                ) => false,
                                (
                                    StoredActiveTurnPhase::AwaitingChild { .. },
                                    _,
                                ) => false,
                                (
                                    StoredActiveTurnPhase::AwaitingToolRecovery { .. },
                                    _,
                                ) => false,
                                (
                                    StoredActiveTurnPhase::AwaitingRunnerRecovery { .. } | StoredActiveTurnPhase::AwaitingCredentialAvailability { .. },
                                    _,
                                ) => false,
                                (
                                    StoredActiveTurnPhase::AwaitingModelCallRecovery {
                                        call, ..
                                    },
                                    crate::ModelCallReconstitutionState::Terminal(
                                        ModelCallDisposition::Ambiguous,
                                    ),
                                ) => *call == model_call.id(),
                                _ => false,
                            }
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                        terminal_execution: Some(execution),
                        ..
                    } => {
                        model_call.attempt() == execution.ended_attempt
                            && execution.ended_call == Some(model_call.id())
                            && matches!(
                                model_call.state(),
                                crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::KnownFailed
                                        | ModelCallDisposition::Cancelled
                                )
                            )
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                        terminal_execution: None,
                        ..
                    } => false,
                    AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                        completing_attempt,
                        completing_call,
                        ..
                    } => {
                        model_call.attempt() == *completing_attempt
                            && model_call.id() == *completing_call
                            && model_call.state()
                                == crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::Completed,
                                )
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                        refusing_attempt,
                        refusing_call,
                        ..
                    } => {
                        model_call.attempt() == *refusing_attempt
                            && model_call.id() == *refusing_call
                            && model_call.state()
                                == crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::Refused,
                                )
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                        terminal_execution,
                        ..
                    } => {
                        model_call.attempt() == terminal_execution.ended_attempt
                            && terminal_execution.ended_call == Some(model_call.id())
                            && model_call.state()
                                == crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::Cancelled,
                                )
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                        reconciling_attempt,
                        ambiguous_call,
                        ..
                    } => {
                        model_call.attempt() == *reconciling_attempt
                            && model_call.id() == *ambiguous_call
                            && model_call.state()
                                == crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::Ambiguous,
                                )
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                        tool_batch,
                        ..
                    } => {
                        model_call.id() == tool_batch.producing_call()
                            && model_call.state()
                                == crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::Completed,
                                )
                    }
                };
                    model_call.turn() == consumed.source_turn
                        && semantic_source_turn == consumed.source_turn
                        && model_call.selection()
                            == *record.origin_configuration.effective().model()
                        && lifecycle_matches
                });
        if accepted_input_turns.contains_key(&accepted_input)
            || !source_record_matches
            || !model_call_matches
            || earlier_reclassified
            || (source_is_active && !active_tail_matches)
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        }
        consumed_by_call.entry(*call).or_default().push((
            consumed.acceptance_position,
            entry,
            accepted_input,
        ));
    }
    for consumed in &input.delegated_consumed_steering {
        let accepted_input = consumed.accepted_input.id();
        if consumed.session != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringSessionMismatch {
                    accepted_input,
                },
            );
        }
        if consumed_inputs.contains_key(&accepted_input) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateConsumedSteering {
                    accepted_input,
                },
            );
        }
        let Some((_, semantic_source_turn)) = steering_by_input.remove(&accepted_input) else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        };
        let AcceptedInputDisposition::ConsumedAsSteering { call } =
            consumed.accepted_input.disposition()
        else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        };
        consumed_inputs.insert(accepted_input, *call);
        if semantic_source_turn != consumed.source_turn
            || accepted_input_turns.contains_key(&accepted_input)
            || records_by_turn.contains_key(&consumed.source_turn)
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        }
    }
    if let Some((_, (entry, _))) = steering_by_input.first_key_value() {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::SteeringSemanticEntryMismatch {
                entry: entry.entry(),
            },
        );
    }
    if let Some(call) = steering_round_evidence
        .keys()
        .find(|call| !consumed_by_call.contains_key(call))
        .copied()
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::SteeringContinuationRoundMismatch {
                call,
            },
        );
    }
    let mut consumed_model_calls = BTreeSet::new();
    let mut consumed_snapshots = BTreeSet::new();
    for (call, mut consumed_entries) in consumed_by_call {
        consumed_entries.sort_unstable_by_key(|(position, _, _)| *position);
        let Some((_, _, first_accepted_input)) = consumed_entries.first().copied() else {
            continue;
        };
        if consumed_entries
            .windows(2)
            .any(|entries| entries[0].0 == entries[1].0)
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input: first_accepted_input,
                },
            );
        }
        let model_call = model_call_inputs[&call];
        let record = records_by_turn[&model_call.turn()];
        let starting_frontier = match record.state {
            AcceptedInputTurnSchedulingRecordState::Queued
            | AcceptedInputTurnSchedulingRecordState::Retired => None,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_frontier, ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_frontier, ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                starting_frontier, ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                starting_frontier, ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_frontier, ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                starting_frontier,
                ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                starting_frontier,
                ..
            } => Some(starting_frontier),
        };
        // Without round evidence the call was prepared at turn start, so its
        // frontier is exactly the starting frontier plus the consumed suffix.
        // With round evidence the call was prepared at a tool-round
        // continuation boundary, so everything before the consumed suffix
        // must be exactly one completed round's result projection.
        let exact_frontier_matches = starting_frontier
            .and_then(|frontier| snapshots.get(&frontier))
            .zip(snapshots.get(&model_call.frontier()))
            .is_some_and(
                |(starting, call_snapshot)| match steering_round_evidence.get(&call) {
                    None => starting
                        .ordered_entries()
                        .chain(consumed_entries.iter().map(|(_, entry, _)| *entry))
                        .eq(call_snapshot.ordered_entries()),
                    Some(round) => {
                        let Some(base_entry_count) = call_snapshot
                            .entry_count()
                            .checked_sub(consumed_entries.len())
                        else {
                            return false;
                        };
                        // The round's tools were issued by the same
                        // continuation attempt that owns the consuming call.
                        round
                            .round_tool_attempts()
                            .iter()
                            .all(|attempt| attempt.issuing_attempt() == model_call.attempt())
                            && call_snapshot
                                .ordered_entries_range(
                                    base_entry_count,
                                    call_snapshot.entry_count(),
                                )
                                .eq(consumed_entries.iter().map(|(_, entry, _)| *entry))
                            && tool_round_continuation_producing_call(
                                model_call.turn(),
                                call_snapshot,
                                base_entry_count,
                                round.round_tool_attempts(),
                                round.round_tool_denials(),
                                &input.inadmissible_requests,
                                &model_calls,
                                &assistant_by_call,
                                &snapshots,
                                &semantic_entries,
                            )
                            .is_some()
                    }
                },
            );
        if !exact_frontier_matches {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input: first_accepted_input,
                },
            );
        }
        consumed_model_calls.insert(call);
        consumed_snapshots.insert(model_call.frontier());
    }
    if let Some(turn) = pinned_targets
        .keys()
        .find(|turn| !referenced_pinned_targets.contains(turn))
        .copied()
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedPinnedTarget { turn },
        );
    }
    let mut referenced_model_calls = consumed_model_calls;
    let mut assistant_call_snapshots = BTreeSet::new();
    for (call, entries) in &assistant_by_call {
        let Some(first_entry) = entries.first().copied() else {
            continue;
        };
        let Some(reconstituted) = model_calls.get(call) else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SemanticEntryCallMissing {
                    entry: first_entry.entry(),
                    call: *call,
                },
            );
        };
        let ReconstitutedModelCall::Ended(ended) = reconstituted else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SemanticEntryCallMismatch {
                    entry: first_entry.entry(),
                    call: *call,
                },
            );
        };
        let call_snapshot = snapshots.get(&ended.frontier().snapshot());
        let accepted_turn_matches =
            records_by_turn
                .get(&ended.turn())
                .copied()
                .is_some_and(|record| {
                    let starting_frontier = match record.state {
                        AcceptedInputTurnSchedulingRecordState::Queued | AcceptedInputTurnSchedulingRecordState::Retired => None,
                        AcceptedInputTurnSchedulingRecordState::Active {
                            starting_frontier, ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                            starting_frontier, ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                            starting_frontier, ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                            starting_frontier, ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                            starting_frontier, ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                            starting_frontier,
                            ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                            starting_frontier,
                            ..
                        } => Some(starting_frontier),
                    };
                    ended.selection() == *record.origin_configuration.effective().model()
                        && starting_frontier
                            .and_then(|starting| snapshots.get(&starting))
                            .zip(call_snapshot)
                            .is_some_and(|(starting, call_snapshot)| {
                                starting.is_semantic_prefix_of(call_snapshot)
                            })
                });
        let delegated_turn_matches = delegated_turns
            .get(&ended.turn())
            .is_some_and(|fact| ended.selection().selected_direct() == fact.selected())
            && !records_by_turn.contains_key(&ended.turn())
            && call_snapshot.is_some();
        let response_matches_disposition = match ended.disposition() {
            ModelCallDisposition::Completed => true,
            ModelCallDisposition::Refused => entries.iter().all(|entry| {
                matches!(
                    semantic_entries
                        .get(entry)
                        .map(SemanticTranscriptEntry::payload),
                    Some(InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call,
                        ..
                    }) if *producing_call == *call
                )
            }),
            ModelCallDisposition::KnownFailed
            | ModelCallDisposition::Cancelled
            | ModelCallDisposition::Ambiguous => false,
        };
        if !response_matches_disposition || (!accepted_turn_matches && !delegated_turn_matches) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SemanticEntryCallMismatch {
                    entry: first_entry.entry(),
                    call: *call,
                },
            );
        }
        referenced_model_calls.insert(*call);
        assistant_call_snapshots.insert(ended.frontier().snapshot());
    }

    let mut turns = Vec::with_capacity(total_order.len());
    let mut previous_terminal = None;
    let mut previous_selected = None;
    let mut active = None;
    let mut active_model_call_recovery = None;
    let mut active_stop_requested_frontier = None;
    let mut active_tool_recovery_attempt = None;
    let mut active_tool_recovery_frontier = None;
    let mut active_executing_tool_batch = None;
    let mut queued_seen = false;
    let mut referenced_snapshots = consumed_snapshots;
    if imported_session.is_some() {
        referenced_snapshots.extend(initial_seed_frontier);
    }
    referenced_snapshots.extend(
        preceding_non_accepted_terminals
            .values()
            .map(|(snapshot, _)| snapshot.frontier().snapshot()),
    );
    let mut attempt_owners = BTreeMap::new();
    let mut claimed_continuation_rounds = BTreeSet::new();

    for (index, turn) in total_order
        .into_iter()
        .filter(|turn| {
            !matches!(
                records_by_turn[turn].state,
                AcceptedInputTurnSchedulingRecordState::Retired
            )
        })
        .enumerate()
    {
        let record = records_by_turn[&turn];
        let external_predecessor = preceding_non_accepted_successors.get(&turn).copied();
        if let Some(predecessor) = external_predecessor {
            let (snapshot, selected) = &preceding_non_accepted_terminals[&predecessor];
            previous_terminal = Some((predecessor, snapshot.clone()));
            previous_selected = Some(*selected);
        }
        let selected = record
            .origin_configuration
            .effective()
            .model()
            .selected_direct();
        let is_queued = matches!(
            &record.state,
            AcceptedInputTurnSchedulingRecordState::Queued
        );
        let model_identity_entry = if !record.model_identity_boundary_required {
            if model_identity_by_turn.contains_key(&turn) {
                return Err(
                    AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch { turn },
                );
            }
            None
        } else if is_queued {
            None
        } else {
            match previous_selected {
                Some(previous) if previous != selected => {
                    Some(model_identity_by_turn.get(&turn).copied().ok_or(
                        AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
                            turn,
                        },
                    )?)
                }
                _ => {
                    if model_identity_by_turn.contains_key(&turn) {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
                                turn,
                            },
                        );
                    }
                    None
                }
            }
        };
        let state = match &record.state {
            AcceptedInputTurnSchedulingRecordState::Retired => continue,
            AcceptedInputTurnSchedulingRecordState::Queued => {
                queued_seen = true;
                ReconstitutedSchedulingState::Queued
            }
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage,
                starting_frontier,
                phase,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                active = Some(turn);
                if phase.owning_turn != turn {
                    return match phase.current_attempt {
                        Some(attempt) => Err(
                            AcceptedInputSchedulingReconstitutionFailure::CurrentAttemptOwnershipMismatch {
                                turn,
                                attempt,
                            },
                        ),
                        None => Err(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        ),
                    };
                }
                if let Some(current_attempt) = phase.current_attempt
                    && attempt_owners.insert(current_attempt, turn).is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                            attempt: current_attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &input.runner_placement_frontiers,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let canonical_phase = match &phase.state {
                    StoredActiveTurnPhase::Prepared | StoredActiveTurnPhase::Running => {
                        phase.canonical_evidence_free_phase().ok_or(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        )?
                    }
                    StoredActiveTurnPhase::AwaitingApproval { wait } => {
                        if wait.session() != session || wait.turn() != turn {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        }
                        phase.canonical_evidence_free_phase().ok_or(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        )?
                    }
                    StoredActiveTurnPhase::AwaitingChild { wait } => {
                        if phase.current_attempt.is_some() {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        }
                        ActiveTurnPhase::AwaitingChild { wait: *wait }
                    }
                    StoredActiveTurnPhase::AwaitingCredentialAvailability { wait } => {
                        let snapshot = snapshots.get(&wait.frontier()).ok_or(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn, accepted_input: record.accepted_input.id(),
                            },
                        )?;
                        if phase.current_attempt.is_some() || !snapshots[starting_frontier].is_semantic_prefix_of(snapshot) {
                            return Err(AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn, accepted_input: record.accepted_input.id(),
                            });
                        }
                        referenced_snapshots.insert(wait.frontier());
                        ActiveTurnPhase::AwaitingCredentialAvailability { wait: *wait }
                    }
                    StoredActiveTurnPhase::AwaitingRunnerRecovery {
                        source_frontier,
                        ..
                    } => {
                        if let Some(source_frontier) = source_frontier {
                            let source_snapshot = snapshots.get(source_frontier).ok_or(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            )?;
                            if !snapshots[starting_frontier]
                                .is_semantic_prefix_of(source_snapshot)
                            {
                                return Err(
                                    AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                        turn,
                                        accepted_input: record.accepted_input.id(),
                                    },
                                );
                            }
                            referenced_snapshots.insert(*source_frontier);
                        }
                        phase.canonical_evidence_free_phase().ok_or(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        )?
                    }
                    StoredActiveTurnPhase::AwaitingToolRecovery { wait, attempt_end } => {
                        let Some(current_attempt) = phase.current_attempt else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        };
                        if wait.session() != session
                            || wait.turn() != turn
                            || wait.issuing_attempt() != current_attempt
                            || !terminal_attempt_end_matches(
                                attempt_end,
                                session,
                                turn,
                                &records_by_turn,
                                &[
                                    UnstoppedAttemptDisposition::Ambiguous,
                                    UnstoppedAttemptDisposition::Lost,
                                ],
                                &[
                                    CancellationStopDisposition::Ambiguous,
                                    CancellationStopDisposition::Lost,
                                ],
                            )
                        {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        }
                        let Ok(running_attempt) =
                            CurrentTurnAttempt::prepared(current_attempt).begin_running()
                        else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        };
                        let canonical_end = match attempt_end.end() {
                            AttemptEnd::WithoutStop { disposition } => {
                                running_attempt.end_without_stop(*disposition)
                            }
                            AttemptEnd::AfterCancellation { cause, disposition } => running_attempt
                                .request_cancellation(*cause)
                                .and_then(|attempt| {
                                    attempt.end_after_cancellation(*cause, *disposition)
                                }),
                            AttemptEnd::AfterFatalMismatch { .. } => {
                                return Err(
                                    AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                        turn,
                                        accepted_input: record.accepted_input.id(),
                                    },
                                );
                            }
                        };
                        let Ok(canonical_end) = canonical_end else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        };
                        active_tool_recovery_attempt = Some(canonical_end);
                        active_tool_recovery_frontier = Some(wait.yielded_frontier());
                        referenced_snapshots.insert(wait.yielded_frontier());
                        ActiveTurnPhase::AwaitingRecoveryDecision {
                            ambiguous_operations: NonEmptyIssuedOperationRefs::singleton(
                                crate::IssuedOperationRef::ToolAttempt(wait.attempt()),
                            ),
                            applied_interrupt: attempt_end
                                .interrupt()
                                .map(|interrupt| interrupt.proof()),
                        }
                    }
                    StoredActiveTurnPhase::StopRequested { call, interrupt } => {
                        let Some(current_attempt) = phase.current_attempt else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        };
                        let successor = records_by_turn.get(&interrupt.successor());
                        let Some(ReconstitutedModelCall::Current(current_call)) =
                            model_calls.get(call)
                        else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMissing {
                                    turn,
                                    call: *call,
                                },
                            );
                        };
                        let Some(pinned) = pinned_targets.get(&turn) else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::PinnedTargetMissing {
                                    call: *call,
                                },
                            );
                        };
                        let call_snapshot = snapshots.get(&current_call.frontier().snapshot());
                        if interrupt.session() != session
                            || interrupt.proof().predecessor() != turn
                            || successor.is_none_or(|successor| {
                                successor.stored_session != session
                                    || successor.turn != interrupt.successor()
                                    || successor.accepted_input.id()
                                        != interrupt.accepted_input()
                                    || successor.order != interrupt.successor_order()
                            })
                            || current_call.turn() != turn
                            || current_call.attempt() != current_attempt
                            || current_call.state()
                                != crate::CurrentModelCallState::CancellationRequested
                            || current_call.selection()
                                != *record.origin_configuration.effective().model()
                            || current_call.target() != pinned.target()
                            || call_snapshot.is_none_or(|snapshot| {
                                !snapshots[starting_frontier].is_semantic_prefix_of(snapshot)
                            })
                        {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        }
                        if current_call.frontier().snapshot() != *starting_frontier {
                            referenced_snapshots.insert(current_call.frontier().snapshot());
                        }
                        active_stop_requested_frontier =
                            Some(current_call.frontier().snapshot());
                        referenced_model_calls.insert(*call);
                        phase.canonical_evidence_free_phase().ok_or(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        )?
                    }
                    StoredActiveTurnPhase::AwaitingModelCallRecovery {
                        call,
                        attempt_end,
                    } => {
                        let Some(current_attempt) = phase.current_attempt else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                },
                            );
                        };
                        let Some(ReconstitutedModelCall::Ended(ended_call)) = model_calls.get(call)
                        else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMissing {
                                    turn,
                                    call: *call,
                                },
                            );
                        };
                        let source_snapshot = snapshots
                            .get(&ended_call.frontier().snapshot())
                            .cloned()
                            .ok_or(
                                AcceptedInputSchedulingReconstitutionFailure::ModelCallSnapshotMissing {
                                    call: *call,
                                },
                            )?;
                        if ended_call.turn() != turn
                            || ended_call.attempt() != current_attempt
                            || ended_call.selection()
                                != *record.origin_configuration.effective().model()
                            || !snapshots[starting_frontier]
                                .is_semantic_prefix_of(&source_snapshot)
                            || ended_call.disposition() != ModelCallDisposition::Ambiguous
                        {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                },
                            );
                        }
                        // An ambiguous continuation call names no
                        // starting-frontier or otherwise-referenced snapshot:
                        // its whole frontier must be the completed round's
                        // result projection, which the recovery wait extends
                        // by no entry.
                        let named_call_frontier_accounted = ended_call.frontier().snapshot()
                            == *starting_frontier
                            || referenced_model_calls.contains(call);
                        if !named_call_frontier_accounted {
                            let continuation_call_matches = continuation_round_evidence
                                .get(call)
                                .is_some_and(|round| {
                                    round.round_tool_attempts().iter().all(|tool_attempt| {
                                        tool_attempt.issuing_attempt() == current_attempt
                                    }) && tool_round_continuation_producing_call(
                                        turn,
                                        &source_snapshot,
                                        source_snapshot.entry_count(),
                                        round.round_tool_attempts(),
                                        round.round_tool_denials(),
                                        &input.inadmissible_requests,
                                        &model_calls,
                                        &assistant_by_call,
                                        &snapshots,
                                        &semantic_entries,
                                    )
                                    .is_some()
                                });
                            if !continuation_call_matches {
                                return Err(
                                    AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                        turn,
                                    },
                                );
                            }
                            claimed_continuation_rounds.insert(*call);
                        }
                        let Ok(running_attempt) =
                            CurrentTurnAttempt::prepared(current_attempt).begin_running()
                        else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                },
                            );
                        };
                        let end_matches = terminal_attempt_end_matches(
                            attempt_end,
                            session,
                            turn,
                            &records_by_turn,
                            &[
                                UnstoppedAttemptDisposition::Ambiguous,
                                UnstoppedAttemptDisposition::Lost,
                            ],
                            &[
                                CancellationStopDisposition::Ambiguous,
                                CancellationStopDisposition::Lost,
                            ],
                        );
                        let canonical_end = match attempt_end.end() {
                            AttemptEnd::WithoutStop { disposition } => {
                                running_attempt.end_without_stop(*disposition)
                            }
                            AttemptEnd::AfterCancellation { cause, disposition } => {
                                running_attempt
                                    .request_cancellation(*cause)
                                    .and_then(|attempt| {
                                        attempt.end_after_cancellation(
                                            *cause,
                                            *disposition,
                                        )
                                    })
                            }
                            AttemptEnd::AfterFatalMismatch { .. } => {
                                return Err(
                                    AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                        turn,
                                    },
                                );
                            }
                        };
                        let Ok(canonical_end) = canonical_end else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                },
                            );
                        };
                        if !end_matches {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                },
                            );
                        }
                        let ambiguous_operations =
                            NonEmptyIssuedOperationRefs::try_from_operations([
                                crate::IssuedOperationRef::ModelCall(*call),
                            ])
                            .map_err(|_| {
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                }
                            })?;
                        referenced_model_calls.insert(*call);
                        if ended_call.frontier().snapshot() != *starting_frontier {
                            referenced_snapshots.insert(ended_call.frontier().snapshot());
                        }
                        active_model_call_recovery = Some(ActiveModelCallRecoveryWait {
                            call: ended_call.clone(),
                            attempt: canonical_end,
                            source_snapshot,
                        });
                        ActiveTurnPhase::AwaitingRecoveryDecision {
                            ambiguous_operations,
                            applied_interrupt: attempt_end
                                .interrupt()
                                .map(|interrupt| interrupt.proof()),
                        }
                    }
                };
                if let Some(tool_batch) = &phase.executing_tool_batch {
                    let yielded_frontier = tool_batch.yielded_snapshot.frontier().snapshot();
                    let stored_yielded = snapshots.get(&yielded_frontier);
                    let producing = model_calls.get(&tool_batch.producing_call);
                    let source = producing.and_then(|call| match call {
                        crate::ReconstitutedModelCall::Ended(call) => {
                            snapshots.get(&call.frontier().snapshot())
                        }
                        crate::ReconstitutedModelCall::Current(_) => None,
                    });
                    let mut observed_requests = Vec::new();
                    let suffix_valid =
                        source.is_some_and(|source| {
                            source.is_semantic_prefix_of(&tool_batch.yielded_snapshot)
                                && source.entry_count() < tool_batch.yielded_snapshot.entry_count()
                                && tool_batch
                                    .yielded_snapshot
                                    .ordered_entries()
                                    .skip(source.entry_count())
                                    .all(|reference| {
                                        let Some(entry) = semantic_entries.get(&reference) else {
                                            return false;
                                        };
                                        match entry.payload() {
                                        SemanticTranscriptEntryPayload::AssistantText {
                                            producing_call,
                                            ..
                                        } => *producing_call == tool_batch.producing_call,
                                        SemanticTranscriptEntryPayload::ProviderCompaction {
                                            producing_call,
                                            ..
                                        } => *producing_call == tool_batch.producing_call,
                                        SemanticTranscriptEntryPayload::ProviderReasoning {
                                            producing_call,
                                            ..
                                        } => *producing_call == tool_batch.producing_call,
                                        SemanticTranscriptEntryPayload::AssistantToolUse {
                                            producing_call,
                                            request,
                                        } if *producing_call == tool_batch.producing_call => {
                                            observed_requests.push(*request);
                                            true
                                        }
                                        SemanticTranscriptEntryPayload::AssistantToolUse { .. }
                                        | SemanticTranscriptEntryPayload::DelegatedTask { .. }
                                        | SemanticTranscriptEntryPayload::DelegationMessage { .. }
                                        | SemanticTranscriptEntryPayload::DelegationResult { .. }
                                        | SemanticTranscriptEntryPayload::Imported { .. }
                                        | SemanticTranscriptEntryPayload::ModelIdentityChanged {
                                            ..
                                        }
                                        | SemanticTranscriptEntryPayload::ContextSummary { .. }
                                        | SemanticTranscriptEntryPayload::OriginAcceptedInput {
                                            ..
                                        }
                                        | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                                            ..
                                        }
                                        | SemanticTranscriptEntryPayload::ToolExecutionResult {
                                            ..
                                        }
                                        | SemanticTranscriptEntryPayload::ToolDenied { .. }
                                        | SemanticTranscriptEntryPayload::ToolInadmissible { .. }
                                        | SemanticTranscriptEntryPayload::ToolClosed { .. }
                                        | SemanticTranscriptEntryPayload::TurnCompleted { .. }
                                        | SemanticTranscriptEntryPayload::TurnFailed { .. }
                                        | SemanticTranscriptEntryPayload::RunnerPlacementChanged { .. }
                                        | SemanticTranscriptEntryPayload::TurnCancelled { .. } => {
                                            false
                                        }
                                    }
                                    })
                        });
                    let producing_matches = matches!(
                        producing,
                        Some(crate::ReconstitutedModelCall::Ended(call))
                            if call.turn() == turn
                                && call.disposition() == ModelCallDisposition::Completed
                    );
                    let phase_matches = match (
                        &phase.state,
                        tool_batch.batch_attempt,
                        tool_batch.awaiting_request,
                    ) {
                        (
                            StoredActiveTurnPhase::Prepared | StoredActiveTurnPhase::Running,
                            Some(turn_attempt),
                            None,
                        ) => phase.current_attempt == Some(turn_attempt),
                        (StoredActiveTurnPhase::AwaitingApproval { wait }, None, Some(request)) => {
                            phase.current_attempt.is_none() && wait.request() == request
                        }
                        (StoredActiveTurnPhase::AwaitingChild { wait }, None, Some(request)) => {
                            phase.current_attempt.is_none() && wait.awaiting_request() == request
                        }
                        _ => false,
                    };
                    if !phase_matches
                        || tool_batch.session != session
                        || stored_yielded != Some(&tool_batch.yielded_snapshot)
                        || !producing_matches
                        || !suffix_valid
                        || observed_requests.as_slice() != tool_batch.requests.as_ref()
                    {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        );
                    }
                    referenced_model_calls.insert(tool_batch.producing_call);
                    referenced_snapshots.insert(yielded_frontier);
                    active_executing_tool_batch = Some(ActiveExecutingToolBatchCorrelation {
                        session,
                        turn,
                        producing_call: tool_batch.producing_call,
                        yielded_frontier,
                        turn_attempt: tool_batch.batch_attempt,
                    });
                }
                ReconstitutedSchedulingState::Active {
                    start,
                    phase: canonical_phase,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage,
                starting_frontier,
                terminal_execution,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &input.runner_placement_frontiers,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let mut source_frontier = *starting_frontier;
                let mut named_call_frontier_accounted = true;
                if let Some(execution) = terminal_execution {
                    let attempt = execution.ended_attempt;
                    if execution.owning_turn != turn {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptOwnershipMismatch {
                                turn,
                                attempt,
                            },
                        );
                    }
                    if !terminal_attempt_end_matches(
                        &execution.attempt_end,
                        session,
                        turn,
                        &records_by_turn,
                        &[
                            UnstoppedAttemptDisposition::KnownFailure,
                            UnstoppedAttemptDisposition::Lost,
                        ],
                        &[CancellationStopDisposition::KnownFailure],
                    ) {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                                turn,
                                attempt,
                            },
                        );
                    }
                    if attempt_owners.insert(attempt, turn).is_some() {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                                attempt,
                            },
                        );
                    }
                    if let Some(call_id) = execution.ended_call {
                        let Some(ReconstitutedModelCall::Ended(call)) = model_calls.get(&call_id)
                        else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMissing {
                                    turn,
                                    call: call_id,
                                },
                            );
                        };
                        let Some(pinned) = pinned_targets.get(&turn) else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::PinnedTargetMissing {
                                    call: call_id,
                                },
                            );
                        };
                        let call_disposition_matches = match execution.attempt_end.end() {
                            AttemptEnd::WithoutStop {
                                disposition: UnstoppedAttemptDisposition::KnownFailure,
                            } => matches!(
                                call.disposition(),
                                ModelCallDisposition::KnownFailed | ModelCallDisposition::Cancelled
                            ),
                            AttemptEnd::WithoutStop {
                                disposition: UnstoppedAttemptDisposition::Lost,
                            }
                            | AttemptEnd::AfterCancellation {
                                disposition: CancellationStopDisposition::KnownFailure,
                                ..
                            } => call.disposition() == ModelCallDisposition::KnownFailed,
                            AttemptEnd::WithoutStop { .. }
                            | AttemptEnd::AfterCancellation { .. }
                            | AttemptEnd::AfterFatalMismatch { .. } => false,
                        };
                        if call.turn() != turn
                            || call.attempt() != attempt
                            || call.selection() != *record.origin_configuration.effective().model()
                            || call.target() != pinned.target()
                            || !call_disposition_matches
                        {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                    turn,
                                },
                            );
                        }
                        // A frontier accounted for by no other law must prove
                        // the named call stood at a tool-round continuation
                        // boundary, checked against the terminal frontier
                        // below once it is loaded.
                        named_call_frontier_accounted = call.frontier().snapshot()
                            == *starting_frontier
                            || referenced_model_calls.contains(&call_id);
                        source_frontier = call.frontier().snapshot();
                        if source_frontier != *starting_frontier {
                            referenced_snapshots.insert(source_frontier);
                        }
                        referenced_model_calls.insert(call_id);
                    }
                }
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                if !referenced_snapshots.insert(*terminal_frontier) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                let failed_entry = failure_by_turn.get(&turn).copied().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::MissingFailureEntry { turn },
                )?;
                let ordinary_terminal_matches =
                    terminal.has_semantic_prefix_and_suffix(source, std::iter::once(failed_entry));
                // A failed continuation call names no starting-frontier or
                // otherwise-referenced snapshot: its whole frontier must be
                // the completed round's result projection that the terminal
                // marker extends by exactly one entry.
                if !named_call_frontier_accounted {
                    let continuation_call_matches = ordinary_terminal_matches
                        && terminal_execution.as_ref().is_some_and(|execution| {
                            execution
                                .terminal_tool_attempts()
                                .iter()
                                .all(|tool_attempt| {
                                    tool_attempt.issuing_attempt() == execution.ended_attempt
                                })
                                && tool_round_continuation_producing_call(
                                    turn,
                                    &terminal,
                                    terminal.entry_count().saturating_sub(1),
                                    execution.terminal_tool_attempts(),
                                    execution.terminal_tool_denials(),
                                    &input.inadmissible_requests,
                                    &model_calls,
                                    &assistant_by_call,
                                    &snapshots,
                                    &semantic_entries,
                                )
                                .is_some()
                        });
                    if !continuation_call_matches {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                turn,
                            },
                        );
                    }
                }
                let tool_round_terminal_matches =
                    terminal_execution.as_ref().is_some_and(|execution| {
                        execution.ended_call.is_none()
                            && tool_round_terminal_producing_call(
                                turn,
                                &terminal,
                                failed_entry,
                                execution.terminal_tool_attempts(),
                                execution.terminal_tool_denials(),
                                &input.inadmissible_requests,
                                &model_calls,
                                &assistant_by_call,
                                &snapshots,
                                &semantic_entries,
                            )
                            .is_some()
                    });
                if !ordinary_terminal_matches && !tool_round_terminal_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalFailed {
                    start,
                    terminal_frontier: terminal,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                starting_lineage,
                starting_frontier,
                completing_attempt,
                completing_attempt_end,
                completing_call,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                if attempt_owners.insert(*completing_attempt, turn).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                            attempt: *completing_attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &input.runner_placement_frontiers,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let Some(ReconstitutedModelCall::Ended(call)) = model_calls.get(completing_call)
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMissing {
                            turn,
                            call: *completing_call,
                        },
                    );
                };
                if call.turn() != turn
                    || call.attempt() != *completing_attempt
                    || !terminal_attempt_end_matches(
                        completing_attempt_end,
                        session,
                        turn,
                        &records_by_turn,
                        &[
                            UnstoppedAttemptDisposition::TurnCompleted,
                            UnstoppedAttemptDisposition::Lost,
                        ],
                        &[
                            CancellationStopDisposition::TurnCompleted,
                            CancellationStopDisposition::Lost,
                        ],
                    )
                    || call.selection() != *record.origin_configuration.effective().model()
                    || call.disposition() != ModelCallDisposition::Completed
                    || (call.frontier().snapshot() != *starting_frontier
                        && !referenced_model_calls.contains(completing_call))
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                let source_frontier = call.frontier().snapshot();
                if source_frontier != *starting_frontier {
                    referenced_snapshots.insert(source_frontier);
                }
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                referenced_model_calls.insert(*completing_call);
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                if !referenced_snapshots.insert(*terminal_frontier) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                let completion_entry = completion_by_turn.get(&turn).copied().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::MissingCompletionEntry { turn },
                )?;
                let assistant_entries = assistant_by_call
                    .get(completing_call)
                    .cloned()
                    .unwrap_or_default();
                if !completed_terminal_matches(
                    source,
                    &terminal,
                    *completing_call,
                    &assistant_entries,
                    completion_entry,
                    &semantic_entries,
                ) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalCompleted {
                    start,
                    terminal_frontier: terminal,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                starting_lineage,
                starting_frontier,
                refusing_attempt,
                refusing_attempt_end,
                refusing_call,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                if attempt_owners.insert(*refusing_attempt, turn).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                            attempt: *refusing_attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &input.runner_placement_frontiers,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let Some(ReconstitutedModelCall::Ended(call)) = model_calls.get(refusing_call)
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMissing {
                            turn,
                            call: *refusing_call,
                        },
                    );
                };
                if call.turn() != turn
                    || call.attempt() != *refusing_attempt
                    || !terminal_attempt_end_matches(
                        refusing_attempt_end,
                        session,
                        turn,
                        &records_by_turn,
                        &[
                            UnstoppedAttemptDisposition::TurnRefused,
                            UnstoppedAttemptDisposition::Lost,
                        ],
                        &[
                            CancellationStopDisposition::TurnRefused,
                            CancellationStopDisposition::Lost,
                        ],
                    )
                    || call.selection() != *record.origin_configuration.effective().model()
                    || call.disposition() != ModelCallDisposition::Refused
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                let named_call_frontier_accounted = call.frontier().snapshot()
                    == *starting_frontier
                    || referenced_model_calls.contains(refusing_call);
                let source_frontier = call.frontier().snapshot();
                if source_frontier != *starting_frontier {
                    referenced_snapshots.insert(source_frontier);
                }
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                // A refused continuation call names no starting-frontier or
                // otherwise-referenced snapshot: its whole frontier must be
                // the completed round's result projection, which the refusal
                // extends by no entry.
                if !named_call_frontier_accounted {
                    let continuation_call_matches = continuation_round_evidence
                        .get(refusing_call)
                        .is_some_and(|round| {
                            round.round_tool_attempts().iter().all(|tool_attempt| {
                                tool_attempt.issuing_attempt() == *refusing_attempt
                            }) && tool_round_continuation_producing_call(
                                turn,
                                source,
                                source.entry_count(),
                                round.round_tool_attempts(),
                                round.round_tool_denials(),
                                &input.inadmissible_requests,
                                &model_calls,
                                &assistant_by_call,
                                &snapshots,
                                &semantic_entries,
                            )
                            .is_some()
                        });
                    if !continuation_call_matches {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                turn,
                            },
                        );
                    }
                    claimed_continuation_rounds.insert(*refusing_call);
                }
                referenced_model_calls.insert(*refusing_call);
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                let assistant_entries = assistant_by_call
                    .get(refusing_call)
                    .cloned()
                    .unwrap_or_default();
                if !referenced_snapshots.insert(*terminal_frontier)
                    || !refused_terminal_matches(
                        source,
                        &terminal,
                        *refusing_call,
                        &assistant_entries,
                        &semantic_entries,
                    )
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalRefused {
                    start,
                    terminal_frontier: terminal,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_lineage,
                starting_frontier,
                terminal_execution,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                let attempt = terminal_execution.ended_attempt;
                let interrupt = terminal_execution.interrupt;
                let attempt_end = &terminal_execution.attempt_end;
                let successor = records_by_turn.get(&interrupt.successor());
                let attempt_end_matches = match attempt_end.end() {
                    AttemptEnd::AfterCancellation {
                        cause,
                        disposition: CancellationStopDisposition::Cancelled,
                    } => *cause == interrupt.proof() && attempt_end.interrupt() == Some(interrupt),
                    AttemptEnd::WithoutStop {
                        disposition: UnstoppedAttemptDisposition::YieldedToDurableWait,
                    } => attempt_end.interrupt() == Some(interrupt),
                    _ => false,
                };
                if terminal_execution.owning_turn != turn
                    || interrupt.session() != session
                    || interrupt.proof().predecessor() != turn
                    || !attempt_end_matches
                    || successor.is_none_or(|successor| {
                        successor.stored_session != session
                            || successor.accepted_input.id() != interrupt.accepted_input()
                            || successor.order != interrupt.successor_order()
                    })
                    || attempt_owners.insert(attempt, turn).is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                            turn,
                            attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &input.runner_placement_frontiers,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let (source_frontier, named_tool_round_producer, named_call_frontier_accounted) =
                    match terminal_execution.ended_call {
                        Some(call_id) => {
                            let Some(ReconstitutedModelCall::Ended(call)) =
                                model_calls.get(&call_id)
                            else {
                                return Err(
                                AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMissing {
                                    turn,
                                    call: call_id,
                                },
                            );
                            };
                            let Some(pinned) = pinned_targets.get(&turn) else {
                                return Err(
                                AcceptedInputSchedulingReconstitutionFailure::PinnedTargetMissing {
                                    call: call_id,
                                },
                            );
                            };
                            // Direct cancellation names its own cancelled call; a
                            // cancellation that terminalized a tool round names the
                            // batch's completed producing call instead.
                            let named_tool_round_producer = match call.disposition() {
                                ModelCallDisposition::Cancelled => None,
                                ModelCallDisposition::Completed => Some(call_id),
                                ModelCallDisposition::KnownFailed
                                | ModelCallDisposition::Refused
                                | ModelCallDisposition::Ambiguous => {
                                    return Err(
                                    AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                        turn,
                                    },
                                );
                                }
                            };
                            if call.turn() != turn
                                || call.attempt() != attempt
                                || call.selection()
                                    != *record.origin_configuration.effective().model()
                                || call.target() != pinned.target()
                            {
                                return Err(
                                AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                    turn,
                                },
                            );
                            }
                            // A frontier accounted for by no other law must prove
                            // the named cancelled call stood at a tool-round
                            // continuation boundary, checked against the terminal
                            // frontier below once it is loaded.
                            let named_call_frontier_accounted = call.frontier().snapshot()
                                == *starting_frontier
                                || referenced_model_calls.contains(&call_id);
                            referenced_model_calls.insert(call_id);
                            if call.frontier().snapshot() != *starting_frontier {
                                referenced_snapshots.insert(call.frontier().snapshot());
                            }
                            (
                                call.frontier().snapshot(),
                                named_tool_round_producer,
                                named_call_frontier_accounted,
                            )
                        }
                        None => (*starting_frontier, None, true),
                    };
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                if !referenced_snapshots.insert(*terminal_frontier) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                let cancellation_entry = cancellation_by_turn.get(&turn).copied().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::MissingCancellationEntry { turn },
                )?;
                let ordinary_terminal_matches = named_tool_round_producer.is_none()
                    && terminal.has_semantic_prefix_and_suffix(
                        source,
                        std::iter::once(cancellation_entry),
                    );
                // A cancelled continuation call names no starting-frontier or
                // otherwise-referenced snapshot: its whole frontier must be
                // the completed round's result projection that the terminal
                // marker extends by exactly one entry.
                if !named_call_frontier_accounted {
                    let continuation_call_matches = ordinary_terminal_matches
                        && terminal_execution
                            .terminal_tool_attempts()
                            .iter()
                            .all(|tool_attempt| {
                                tool_attempt.issuing_attempt() == terminal_execution.ended_attempt
                            })
                        && tool_round_continuation_producing_call(
                            turn,
                            &terminal,
                            terminal.entry_count().saturating_sub(1),
                            terminal_execution.terminal_tool_attempts(),
                            terminal_execution.terminal_tool_denials(),
                            &input.inadmissible_requests,
                            &model_calls,
                            &assistant_by_call,
                            &snapshots,
                            &semantic_entries,
                        )
                        .is_some();
                    if !continuation_call_matches {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                turn,
                            },
                        );
                    }
                }
                // A stored tool round either names no call, proving a batch
                // interrupt closed an executing round, or names the completed
                // producing call the round's own suffix must identify.
                let tool_round_admissible =
                    terminal_execution.ended_call.is_none() || named_tool_round_producer.is_some();
                let tool_round_terminal_matches = tool_round_admissible
                    && tool_round_terminal_producing_call(
                        turn,
                        &terminal,
                        cancellation_entry,
                        terminal_execution.terminal_tool_attempts(),
                        terminal_execution.terminal_tool_denials(),
                        &input.inadmissible_requests,
                        &model_calls,
                        &assistant_by_call,
                        &snapshots,
                        &semantic_entries,
                    )
                    .is_some_and(|producing_call| {
                        named_tool_round_producer.is_none_or(|named| named == producing_call)
                    });
                if !ordinary_terminal_matches && !tool_round_terminal_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalCancelled {
                    start,
                    terminal_frontier: terminal,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                starting_lineage,
                starting_frontier,
                reconciling_attempt,
                reconciling_attempt_end,
                ambiguous_call,
                authority,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                let authority_matches = match authority {
                    AutomaticReconciliationAuthority::AppliedInterrupt(interrupt) => {
                        let attempt_end_matches = match reconciling_attempt_end.end() {
                            AttemptEnd::WithoutStop {
                                disposition:
                                    UnstoppedAttemptDisposition::Ambiguous
                                    | UnstoppedAttemptDisposition::Lost,
                            } => reconciling_attempt_end.interrupt().is_none(),
                            AttemptEnd::AfterCancellation {
                                cause,
                                disposition:
                                    CancellationStopDisposition::Ambiguous
                                    | CancellationStopDisposition::Lost,
                            } => {
                                *cause == interrupt.proof()
                                    && reconciling_attempt_end.interrupt() == Some(*interrupt)
                            }
                            _ => false,
                        };
                        let successor = records_by_turn.get(&interrupt.successor());
                        interrupt.session() == session
                            && interrupt.proof().predecessor() == turn
                            && attempt_end_matches
                            && successor.is_some_and(|successor| {
                                successor.stored_session == session
                                    && successor.accepted_input.id() == interrupt.accepted_input()
                                    && successor.order == interrupt.successor_order()
                            })
                    }
                    AutomaticReconciliationAuthority::AutomaticRecovery { .. } => {
                        matches!(
                            reconciling_attempt_end.end(),
                            AttemptEnd::WithoutStop {
                                disposition: UnstoppedAttemptDisposition::Ambiguous
                                    | UnstoppedAttemptDisposition::Lost,
                            }
                        ) && reconciling_attempt_end.interrupt().is_none()
                    }
                };
                if !authority_matches || attempt_owners.insert(*reconciling_attempt, turn).is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                            turn,
                            attempt: *reconciling_attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &input.runner_placement_frontiers,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let Some(ReconstitutedModelCall::Ended(call)) = model_calls.get(ambiguous_call)
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMissing {
                            turn,
                            call: *ambiguous_call,
                        },
                    );
                };
                let Some(pinned) = pinned_targets.get(&turn) else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::PinnedTargetMissing {
                            call: *ambiguous_call,
                        },
                    );
                };
                if call.turn() != turn
                    || call.attempt() != *reconciling_attempt
                    || call.selection() != *record.origin_configuration.effective().model()
                    || call.target() != pinned.target()
                    || call.disposition() != ModelCallDisposition::Ambiguous
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                let named_call_frontier_accounted = call.frontier().snapshot()
                    == *starting_frontier
                    || referenced_model_calls.contains(ambiguous_call);
                referenced_model_calls.insert(*ambiguous_call);
                let source_frontier = call.frontier().snapshot();
                if source_frontier != *starting_frontier {
                    referenced_snapshots.insert(source_frontier);
                }
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                // A reconciliation-required continuation call names no
                // starting-frontier or otherwise-referenced snapshot: its
                // whole frontier must be the completed round's result
                // projection, which the reconciliation boundary extends by no
                // entry.
                if !named_call_frontier_accounted {
                    let continuation_call_matches = continuation_round_evidence
                        .get(ambiguous_call)
                        .is_some_and(|round| {
                            round.round_tool_attempts().iter().all(|tool_attempt| {
                                tool_attempt.issuing_attempt() == *reconciling_attempt
                            }) && tool_round_continuation_producing_call(
                                turn,
                                source,
                                source.entry_count(),
                                round.round_tool_attempts(),
                                round.round_tool_denials(),
                                &input.inadmissible_requests,
                                &model_calls,
                                &assistant_by_call,
                                &snapshots,
                                &semantic_entries,
                            )
                            .is_some()
                        });
                    if !continuation_call_matches {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                turn,
                            },
                        );
                    }
                    claimed_continuation_rounds.insert(*ambiguous_call);
                }
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                if !referenced_snapshots.insert(*terminal_frontier)
                    || !terminal.same_semantic_content(source)
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalReconciliationRequired {
                    start,
                    terminal_frontier: terminal,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                starting_lineage,
                starting_frontier,
                reconciling_attempt,
                reconciling_attempt_end,
                tool_batch,
                authority,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                let interrupt = match *authority {
                    AutomaticReconciliationAuthority::AppliedInterrupt(interrupt) => {
                        Some(interrupt)
                    }
                    AutomaticReconciliationAuthority::AutomaticRecovery { .. } => None,
                };
                let attempt_end_matches =
                    tool_reconciliation_attempt_end_matches(reconciling_attempt_end, interrupt);
                let successor_matches = match interrupt {
                    Some(interrupt) => {
                        records_by_turn
                            .get(&interrupt.successor())
                            .is_some_and(|successor| {
                                successor.stored_session == session
                                    && successor.accepted_input.id() == interrupt.accepted_input()
                                    && successor.order == interrupt.successor_order()
                            })
                    }
                    None => true,
                };
                let Some(ambiguous_tool) = tool_batch.awaiting_recovery() else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                            turn,
                            attempt: *reconciling_attempt,
                        },
                    );
                };
                if interrupt.is_some_and(|interrupt| {
                    interrupt.session() != session || interrupt.proof().predecessor() != turn
                }) || !attempt_end_matches
                    || ambiguous_tool.session() != session
                    || ambiguous_tool.turn() != turn
                    || ambiguous_tool.issuing_attempt() != *reconciling_attempt
                    || !successor_matches
                    || attempt_owners.insert(*reconciling_attempt, turn).is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                            turn,
                            attempt: *reconciling_attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &input.runner_placement_frontiers,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let source_frontier = ambiguous_tool.yielded_frontier();
                if source_frontier != *starting_frontier {
                    referenced_snapshots.insert(source_frontier);
                }
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                let entry_ids = terminal
                    .ordered_entries()
                    .skip(source.entry_count())
                    .map(|reference| reference.entry())
                    .collect();
                let expected_projection = tool_batch
                    .prepare_reconciliation_projection(entry_ids, *terminal_frontier)
                    .ok();
                let projection_matches = expected_projection.as_ref().is_some_and(|projection| {
                    projection.snapshot() == &terminal
                        && projection.entries().iter().all(|expected| {
                            semantic_entries.get(&expected.reference()) == Some(expected)
                        })
                });
                if !referenced_snapshots.insert(*terminal_frontier) || !projection_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalReconciliationRequired {
                    start,
                    terminal_frontier: terminal,
                }
            }
        };

        if !matches!(state, ReconstitutedSchedulingState::Queued)
            && !origin_by_turn.contains_key(&turn)
        {
            return Err(AcceptedInputSchedulingReconstitutionFailure::MissingOriginEntry { turn });
        }
        if matches!(state, ReconstitutedSchedulingState::Queued) {
            previous_terminal = None;
        }

        previous_selected = Some(selected);
        turns.push(AcceptedInputTurnSchedulingProjection {
            session,
            turn,
            accepted_input: record.accepted_input.clone(),
            order: record.order,
            origin_configuration: record.origin_configuration.clone(),
            configuration_provenance: record.configuration_provenance.clone(),
            state,
        });
    }

    referenced_snapshots.extend(compaction_snapshots);
    for frontier in &input.runner_placement_frontiers {
        let boundary = snapshots.get(frontier).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedSnapshot {
                snapshot: *frontier,
            },
        )?;
        let valid = boundary
            .ordered_entries()
            .next_back()
            .and_then(|reference| semantic_entries.get(&reference))
            .is_some_and(|entry| {
                entry.source_session() == session
                    && matches!(
                        entry.payload(),
                        SemanticTranscriptEntryPayload::RunnerPlacementChanged { .. }
                    )
            });
        if !valid {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::UnreferencedSnapshot {
                    snapshot: *frontier,
                },
            );
        }
        referenced_snapshots.insert(*frontier);
    }
    referenced_snapshots.extend(assistant_call_snapshots);
    if let Some(snapshot) = snapshots
        .keys()
        .copied()
        .find(|snapshot| !referenced_snapshots.contains(snapshot))
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedSnapshot { snapshot },
        );
    }
    if let Some(call) = model_calls
        .keys()
        .copied()
        .find(|call| !referenced_model_calls.contains(call))
    {
        return Err(AcceptedInputSchedulingReconstitutionFailure::UnreferencedModelCall { call });
    }
    if let Some(call) = continuation_round_evidence
        .keys()
        .copied()
        .find(|call| !claimed_continuation_rounds.contains(call))
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::ContinuationRoundMismatch { call },
        );
    }

    let active_acceptance_tail = reconstitute_active_acceptance_tail(
        session,
        active,
        input.active_acceptance_tail.as_ref(),
        ActiveAcceptanceTailReconstitutionEvidence {
            records_by_turn: &records_by_turn,
            accepted_input_turns: &accepted_input_turns,
            consumed_inputs: &consumed_inputs,
            preceding_non_accepted_terminals: &preceding_non_accepted_terminal_turns,
            execution_position_by_turn: &execution_position_by_turn,
        },
    )?;

    Ok(AcceptedInputSchedulingProjection {
        runner_placement_frontier: input.runner_placement_frontiers.last().copied(),
        session: input.session.clone(),
        initial_seed_frontier,
        latest_compaction_result,
        active_compaction_call,
        turns: turns.into_boxed_slice(),
        active_acceptance_tail,
        semantic_entries,
        snapshots,
        attempt_owners,
        active_model_call_recovery,
        active_stop_requested_frontier,
        active_tool_recovery_attempt,
        active_tool_recovery_frontier,
        active_executing_tool_batch,
        preceding_non_accepted_successors,
        preceding_non_accepted_terminals,
    })
}

pub(super) fn promote_external_interrupt_chains(
    total_order: Vec<TurnId>,
    external_successors: BTreeSet<TurnId>,
    ordinary_roots: &BTreeSet<TurnId>,
    queued_turns: &BTreeSet<TurnId>,
) -> Vec<TurnId> {
    if external_successors.is_empty() {
        return total_order;
    }
    let roots = ordinary_roots
        .union(&external_successors)
        .copied()
        .collect::<BTreeSet<_>>();
    let Some((crossing_start, insertion)) = total_order
        .iter()
        .enumerate()
        .filter(|(_, turn)| external_successors.contains(turn) && queued_turns.contains(turn))
        .find_map(|(start, _)| {
            total_order[..start]
                .iter()
                .position(|turn| queued_turns.contains(turn) && ordinary_roots.contains(turn))
                .map(|insertion| (start, insertion))
        })
    else {
        return total_order;
    };
    let start = total_order[insertion..crossing_start]
        .iter()
        .position(|turn| external_successors.contains(turn))
        .map(|offset| insertion + offset)
        .unwrap_or(crossing_start);
    let end = total_order[crossing_start + 1..]
        .iter()
        .position(|turn| roots.contains(turn))
        .map(|offset| crossing_start + 1 + offset)
        .unwrap_or(total_order.len());
    let mut promoted = Vec::with_capacity(total_order.len());
    promoted.extend_from_slice(&total_order[..insertion]);
    promoted.extend_from_slice(&total_order[start..end]);
    promoted.extend_from_slice(&total_order[insertion..start]);
    promoted.extend_from_slice(&total_order[end..]);
    promoted
}
