//! Turn scheduling acceptance tail for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::correlation::{
    historical_interrupt_matches_terminal_proof, origin_delivery_matches_record,
};
use super::failure::AcceptedInputSchedulingReconstitutionFailure;
use super::projection::{SessionAcceptanceTail, SessionAcceptanceTailEntry};
use super::reconstitution_input::{
    SessionAcceptanceTailEntryState, SessionAcceptanceTailReconstitutionInput,
    StoredActiveTurnPhase,
};
use super::record::{AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState};
use crate::{
    AcceptedInputDisposition, AcceptedInputId, DeliveryRequest, SessionId, SessionInputPosition,
    TurnId,
};
use std::{collections::BTreeMap, collections::BTreeSet};

pub(super) struct ActiveAcceptanceTailReconstitutionEvidence<'a, 'record> {
    pub(super) records_by_turn: &'a BTreeMap<TurnId, &'record AcceptedInputTurnSchedulingRecord>,
    pub(super) accepted_input_turns: &'a BTreeMap<AcceptedInputId, TurnId>,
    pub(super) consumed_inputs: &'a BTreeMap<AcceptedInputId, crate::ModelCallId>,
    pub(super) preceding_non_accepted_terminals: &'a BTreeSet<TurnId>,
    pub(super) execution_position_by_turn: &'a BTreeMap<TurnId, usize>,
}

pub(super) fn reconstitute_active_acceptance_tail(
    session: SessionId,
    active: Option<TurnId>,
    candidate: Option<&SessionAcceptanceTailReconstitutionInput>,
    evidence: ActiveAcceptanceTailReconstitutionEvidence<'_, '_>,
) -> Result<Option<SessionAcceptanceTail>, AcceptedInputSchedulingReconstitutionFailure> {
    let ActiveAcceptanceTailReconstitutionEvidence {
        records_by_turn,
        accepted_input_turns,
        consumed_inputs,
        preceding_non_accepted_terminals,
        execution_position_by_turn,
    } = evidence;
    let (active, candidate) = match (active, candidate) {
        (None, None) => return Ok(None),
        (None, Some(_)) => {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::UnexpectedActiveAcceptanceTail,
            );
        }
        (Some(active), None) => {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::MissingActiveAcceptanceTail {
                    turn: active,
                },
            );
        }
        (Some(active), Some(candidate)) => (active, candidate),
    };

    if candidate.session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailSessionMismatch {
                expected: session,
                actual: candidate.session,
            },
        );
    }

    let active_record = records_by_turn[&active];
    let applied_interrupt = match &active_record.state {
        AcceptedInputTurnSchedulingRecordState::Active { phase, .. } => match &phase.state {
            StoredActiveTurnPhase::StopRequested { interrupt, .. } => Some(*interrupt),
            StoredActiveTurnPhase::AwaitingModelCallRecovery { attempt_end, .. } => {
                attempt_end.interrupt()
            }
            StoredActiveTurnPhase::AwaitingToolRecovery { attempt_end, .. } => {
                attempt_end.interrupt()
            }
            StoredActiveTurnPhase::Prepared
            | StoredActiveTurnPhase::Running
            | StoredActiveTurnPhase::AwaitingApproval { .. }
            | StoredActiveTurnPhase::AwaitingChild { .. }
            | StoredActiveTurnPhase::AwaitingRunnerRecovery { .. } => None,
        },
        AcceptedInputTurnSchedulingRecordState::Queued
        | AcceptedInputTurnSchedulingRecordState::TerminalFailed { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalCompleted { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalRefused { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalCancelled { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired { .. } => None,
    };
    let expected_anchor = active_record.accepted_input.id();
    if candidate.anchor != expected_anchor {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
                turn: active,
                expected: expected_anchor,
                actual: candidate.anchor,
            },
        );
    }

    let latest_known_origin_position = records_by_turn
        .values()
        .map(|record| record.order.acceptance_position())
        .fold(
            active_record.order.acceptance_position(),
            SessionInputPosition::max,
        );
    if latest_known_origin_position > candidate.observed_last_position {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: candidate.observed_last_position,
                actual: Some(latest_known_origin_position),
            },
        );
    }

    if let Some(first) = candidate.entries.first()
        && first.accepted_input.id() != expected_anchor
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
                turn: active,
                expected: expected_anchor,
                actual: first.accepted_input.id(),
            },
        );
    }

    let mut expected_position = active_record.order.acceptance_position();
    let mut pending_steering_seen = false;
    let origin_by_position = records_by_turn
        .values()
        .map(|record| {
            (
                record.order.acceptance_position(),
                record.accepted_input.id(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    let mut entries = Vec::with_capacity(candidate.entries.len());
    for (index, entry) in candidate.entries.iter().enumerate() {
        let accepted_input = entry.accepted_input.id();
        if entry.session != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailEntrySessionMismatch {
                    accepted_input,
                },
            );
        }
        if !seen.insert(accepted_input) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptanceTailEntry {
                    accepted_input,
                },
            );
        }
        if entry.position != expected_position {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailPositionMismatch {
                    accepted_input,
                    expected: expected_position,
                    actual: entry.position,
                },
            );
        }

        let disposition_valid = match entry.state {
            SessionAcceptanceTailEntryState::RetiredGoalOrigin => {
                match entry.accepted_input.disposition() {
                    AcceptedInputDisposition::OriginOf(origin) => {
                        !records_by_turn.contains_key(origin)
                            && !accepted_input_turns.contains_key(&accepted_input)
                            && match entry.delivery {
                                DeliveryRequest::StartWhenNoActiveTurn { .. } => true,
                                DeliveryRequest::Interrupt { .. }
                                | DeliveryRequest::AfterCurrentTurn { .. }
                                | DeliveryRequest::NextSafePoint { .. } => false,
                            }
                    }
                    AcceptedInputDisposition::PendingSteering { .. }
                    | AcceptedInputDisposition::ConsumedAsSteering { .. }
                    | AcceptedInputDisposition::ReclassifiedAsTurnOrigin { .. }
                    | AcceptedInputDisposition::ClosedNotDelivered => false,
                }
            }
            SessionAcceptanceTailEntryState::RuntimeRelevant => {
                match entry.accepted_input.disposition() {
                    AcceptedInputDisposition::OriginOf(origin)
                    | AcceptedInputDisposition::ReclassifiedAsTurnOrigin { turn: origin, .. } => {
                        records_by_turn.get(origin).is_some_and(|record| {
                            record.accepted_input == entry.accepted_input
                                && record.order.acceptance_position() == entry.position
                                && entry.delivery == record.origin_delivery
                                && origin_delivery_matches_record(
                                    record.origin_delivery,
                                    record,
                                    records_by_turn,
                                    preceding_non_accepted_terminals,
                                )
                        })
                    }
                    AcceptedInputDisposition::PendingSteering { binding } => {
                        pending_steering_seen = true;
                        !accepted_input_turns.contains_key(&accepted_input)
                            && !origin_by_position.contains_key(&entry.position)
                            && matches!(
                                entry.delivery,
                                DeliveryRequest::NextSafePoint {
                                    expected_active_turn,
                                } if expected_active_turn == binding.source_turn()
                                    && expected_active_turn == active
                            )
                    }
                    AcceptedInputDisposition::ConsumedAsSteering { call } => {
                        let source_precedes_active = matches!(
                            entry.delivery,
                            DeliveryRequest::NextSafePoint {
                                expected_active_turn,
                            } if records_by_turn.get(&expected_active_turn).is_some_and(|record| {
                                execution_position_by_turn.get(&expected_active_turn)
                                    .zip(execution_position_by_turn.get(&active))
                                    .is_some_and(|(source, active)| source < active)
                                    && !matches!(
                                        record.state,
                                        AcceptedInputTurnSchedulingRecordState::Queued
                                            | AcceptedInputTurnSchedulingRecordState::Active { .. }
                                    )
                            }) || preceding_non_accepted_terminals
                                .contains(&expected_active_turn)
                        );
                        consumed_inputs.get(&accepted_input) == Some(call)
                            && !pending_steering_seen
                            && !accepted_input_turns.contains_key(&accepted_input)
                            && !origin_by_position.contains_key(&entry.position)
                            && matches!(
                                entry.delivery,
                                DeliveryRequest::NextSafePoint {
                                    expected_active_turn,
                                } if expected_active_turn == active || source_precedes_active
                            )
                    }
                    AcceptedInputDisposition::ClosedNotDelivered => {
                        !accepted_input_turns.contains_key(&accepted_input)
                            && !origin_by_position.contains_key(&entry.position)
                            && matches!(
                                entry.delivery,
                                DeliveryRequest::NextSafePoint {
                                    expected_active_turn,
                                } if expected_active_turn == active
                            )
                    }
                }
            }
        };
        if !disposition_valid
            || (index == 0 && entry.accepted_input != active_record.accepted_input)
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                    accepted_input,
                },
            );
        }

        if index > 0
            && let DeliveryRequest::Interrupt {
                expected_active_turn,
                ..
            } = entry.delivery
            && !applied_interrupt.is_some_and(|interrupt| {
                expected_active_turn == active
                    && interrupt.accepted_input() == accepted_input
                    && accepted_input_turns.get(&accepted_input) == Some(&interrupt.successor())
            })
            && !accepted_input_turns
                .get(&accepted_input)
                .and_then(|successor| records_by_turn.get(successor))
                .is_some_and(|successor| {
                    historical_interrupt_matches_terminal_proof(
                        session,
                        active,
                        expected_active_turn,
                        accepted_input,
                        successor,
                        records_by_turn,
                    )
                })
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                    turn: active,
                    accepted_input,
                },
            );
        }

        entries.push(SessionAcceptanceTailEntry {
            accepted_input: entry.accepted_input.clone(),
            position: entry.position,
            delivery: entry.delivery,
        });
        if index + 1 < candidate.entries.len() {
            expected_position = expected_position.checked_next().ok_or(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                    expected: candidate.observed_last_position,
                    actual: Some(entry.position),
                },
            )?;
        }
    }

    let actual_last = entries.last().map(|entry| entry.position);
    if actual_last != Some(candidate.observed_last_position) {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: candidate.observed_last_position,
                actual: actual_last,
            },
        );
    }

    Ok(Some(SessionAcceptanceTail {
        session,
        anchor: expected_anchor,
        observed_last_position: candidate.observed_last_position,
        entries: entries.into_boxed_slice(),
    }))
}
