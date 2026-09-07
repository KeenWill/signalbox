//! Model-call turn closure for `docs/spec/model-call-execution.md`.

use super::{
    AmbiguousModelCallTurn, CancellationFrontierSource, CancelledModelCallTurnIdentities,
    FailedModelCallTurnIdentities, ModelCallClosureError, ModelCallTerminalIdentities,
    ModelCallTerminalObservation, ModelCallTerminalOutcome, ReclassifiedPendingSteeringTurn,
    ReconciliationRequiredModelCallTurn, RefusedModelCallTurn, assemble_stopped_tool_round,
    assemble_tool_round, close_cancelled_turn, close_failed_turn,
    close_failed_turn_after_cancellation, complete_turn,
};
use crate::{
    AssistantResponsePart, CancellationStopDisposition, CurrentModelCall, CurrentTurnAttempt,
    CurrentTurnAttemptState, DangerousToolAutoApproval, NonEmptyIssuedOperationRefs,
    ReconciliationMarker, ResolvedContextFrontierSnapshot, SemanticTranscriptEntry,
    SemanticTranscriptEntryPayload, SessionId, TurnAttemptStopCauses, TurnDisposition, TurnId,
    UnstoppedAttemptDisposition,
};
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
pub(super) struct ModelCallTurnScope {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
}

pub(super) struct ModelCallTerminalContext {
    pub(super) reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
    pub(super) dangerous_tool_auto_approval: DangerousToolAutoApproval,
}

pub(super) fn apply_terminal_observation(
    scope: ModelCallTurnScope,
    attempt: CurrentTurnAttempt,
    call: CurrentModelCall,
    frontier_entries: Box<[SemanticTranscriptEntry]>,
    observation: ModelCallTerminalObservation,
    identities: ModelCallTerminalIdentities,
    context: ModelCallTerminalContext,
) -> Result<ModelCallTerminalOutcome, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    let ModelCallTerminalContext {
        reclassified_pending_steering,
        dangerous_tool_auto_approval,
    } = context;
    let cancellation_proof = match attempt.state() {
        CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::CancellationOnly { interrupt },
        } => Some(*interrupt),
        CurrentTurnAttemptState::Prepared
        | CurrentTurnAttemptState::Running
        | CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::FatalMismatch(_),
        } => None,
    };
    let disposition = observation.disposition();
    let source_frontier = call.frontier();
    let ended_call = call
        .end_classified(disposition)
        .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
    match observation {
        ModelCallTerminalObservation::Completed { assistant_text } => {
            let ModelCallTerminalIdentities::Completed(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = match cancellation_proof {
                Some(proof) => attempt
                    .end_after_cancellation(proof, CancellationStopDisposition::TurnCompleted),
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::TurnCompleted),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let completed = complete_turn(
                scope,
                ended_call,
                ended_attempt,
                frontier_entries.into_vec(),
                assistant_text
                    .into_iter()
                    .map(AssistantResponsePart::Text)
                    .collect(),
                identities,
                reclassified_pending_steering,
            )?;
            Ok(ModelCallTerminalOutcome::Completed(completed))
        }
        ModelCallTerminalObservation::CompletedWithProviderCompaction { response, .. }
        | ModelCallTerminalObservation::CompletedWithProviderReasoning { response } => {
            let ModelCallTerminalIdentities::Completed(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = match cancellation_proof {
                Some(proof) => attempt
                    .end_after_cancellation(proof, CancellationStopDisposition::TurnCompleted),
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::TurnCompleted),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let completed = complete_turn(
                scope,
                ended_call,
                ended_attempt,
                frontier_entries.into_vec(),
                response,
                identities,
                reclassified_pending_steering,
            )?;
            Ok(ModelCallTerminalOutcome::Completed(completed))
        }
        ModelCallTerminalObservation::CompletedWithTools { response, .. } => {
            if let Some(proof) = cancellation_proof {
                let ModelCallTerminalIdentities::StoppedToolRound(identities) = identities else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let ended_attempt = attempt
                    .end_after_cancellation(proof, CancellationStopDisposition::Cancelled)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                return assemble_stopped_tool_round(
                    scope,
                    ended_call,
                    ended_attempt,
                    frontier_entries.into_vec(),
                    response,
                    proof,
                    identities,
                    dangerous_tool_auto_approval,
                    reclassified_pending_steering,
                )
                .map(ModelCallTerminalOutcome::CancelledWithToolResponse);
            }
            let ModelCallTerminalIdentities::ToolRound(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = attempt
                .end_without_stop(UnstoppedAttemptDisposition::YieldedToDurableWait)
                .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            assemble_tool_round(
                scope,
                ended_call,
                ended_attempt,
                frontier_entries.into_vec(),
                response,
                identities,
                dangerous_tool_auto_approval,
            )
            .map(ModelCallTerminalOutcome::ToolRound)
        }
        ModelCallTerminalObservation::KnownFailed => {
            let ModelCallTerminalIdentities::Failed(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                session,
                source_frontier.snapshot(),
                frontier_entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
            let failed = match cancellation_proof {
                Some(proof) => close_failed_turn_after_cancellation(
                    scope,
                    attempt,
                    ended_call,
                    source,
                    proof,
                    identities,
                    reclassified_pending_steering,
                ),
                None => close_failed_turn(
                    scope,
                    attempt,
                    Some(ended_call),
                    source,
                    identities,
                    UnstoppedAttemptDisposition::KnownFailure,
                    reclassified_pending_steering,
                ),
            }?;
            Ok(ModelCallTerminalOutcome::Failed(failed))
        }
        ModelCallTerminalObservation::Cancelled => match cancellation_proof {
            Some(proof) => {
                let ModelCallTerminalIdentities::PhysicalCancellation(identities) = identities
                else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let identities = CancelledModelCallTurnIdentities {
                    cancellation_entry: identities.terminal_entry,
                    terminal_frontier: identities.terminal_frontier,
                    pending_steering_reclassifications: identities
                        .pending_steering_reclassifications,
                };
                let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                    session,
                    source_frontier.snapshot(),
                    frontier_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                close_cancelled_turn(
                    scope,
                    Some(attempt),
                    Some(ended_call),
                    CancellationFrontierSource::new(source, &[]),
                    proof,
                    identities,
                    reclassified_pending_steering,
                )
                .map(ModelCallTerminalOutcome::Cancelled)
            }
            None => {
                let ModelCallTerminalIdentities::PhysicalCancellation(identities) = identities
                else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let identities = FailedModelCallTurnIdentities {
                    failure_entry: identities.terminal_entry,
                    terminal_frontier: identities.terminal_frontier,
                    pending_steering_reclassifications: identities
                        .pending_steering_reclassifications,
                };
                let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                    session,
                    source_frontier.snapshot(),
                    frontier_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                close_failed_turn(
                    scope,
                    attempt,
                    Some(ended_call),
                    source,
                    identities,
                    UnstoppedAttemptDisposition::KnownFailure,
                    reclassified_pending_steering,
                )
                .map(ModelCallTerminalOutcome::Failed)
            }
        },
        observation @ ModelCallTerminalObservation::Refused
        | observation @ ModelCallTerminalObservation::RefusedWithProviderCompaction { .. } => {
            let ModelCallTerminalIdentities::Refused(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = match cancellation_proof {
                Some(proof) => {
                    attempt.end_after_cancellation(proof, CancellationStopDisposition::TurnRefused)
                }
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::TurnRefused),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                session,
                source_frontier.snapshot(),
                frontier_entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
            let provider_compaction =
                if let ModelCallTerminalObservation::RefusedWithProviderCompaction {
                    provider_compaction,
                    ..
                } = observation
                {
                    provider_compaction
                } else {
                    Vec::new()
                };
            if provider_compaction.len() != identities.provider_compaction_entries.len() {
                return Err(ModelCallClosureError::AssistantIdentityCountMismatch);
            }
            let mut used = frontier_entries
                .iter()
                .map(SemanticTranscriptEntry::identity)
                .collect::<BTreeSet<_>>();
            if identities
                .provider_compaction_entries
                .iter()
                .any(|identity| !used.insert(*identity))
            {
                return Err(ModelCallClosureError::FrontierDerivationFailed);
            }
            let provider_compaction_entries = identities
                .provider_compaction_entries
                .into_iter()
                .zip(provider_compaction)
                .map(|(identity, block)| {
                    SemanticTranscriptEntry::from_validated_parts(
                        identity,
                        session,
                        SemanticTranscriptEntryPayload::ProviderCompaction {
                            producing_call: ended_call.id(),
                            block,
                        },
                    )
                })
                .collect::<Vec<_>>();
            let terminal_snapshot = source
                .derive_appending_candidate(
                    identities.terminal_frontier,
                    provider_compaction_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
            Ok(ModelCallTerminalOutcome::Refused(RefusedModelCallTurn {
                session,
                turn,
                call: ended_call,
                attempt: ended_attempt,
                disposition: TurnDisposition::Refused,
                provider_compaction_entries: provider_compaction_entries.into_boxed_slice(),
                terminal_snapshot,
                reclassified_pending_steering,
            }))
        }
        ModelCallTerminalObservation::Ambiguous => {
            let ModelCallTerminalIdentities::Ambiguous(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let call_id = ended_call.id();
            let ended_attempt = match cancellation_proof {
                Some(proof) => {
                    attempt.end_after_cancellation(proof, CancellationStopDisposition::Ambiguous)
                }
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::Ambiguous),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let ambiguous_operations = NonEmptyIssuedOperationRefs::try_from_operations([
                crate::IssuedOperationRef::ModelCall(call_id),
            ])
            .map_err(|_| ModelCallClosureError::AmbiguityConstructionFailed)?;
            if let Some(proof) = cancellation_proof {
                let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                    session,
                    source_frontier.snapshot(),
                    frontier_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                let terminal_snapshot = source
                    .derive_appending_candidate(identities.terminal_frontier, Vec::new())
                    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                let marker =
                    ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations, proof);
                Ok(ModelCallTerminalOutcome::ReconciliationRequired(
                    ReconciliationRequiredModelCallTurn {
                        session,
                        turn,
                        call: ended_call,
                        attempt: ended_attempt,
                        disposition: TurnDisposition::ReconciliationRequired { marker },
                        terminal_snapshot,
                        reclassified_pending_steering,
                    },
                ))
            } else {
                Ok(ModelCallTerminalOutcome::AwaitingRecovery(
                    AmbiguousModelCallTurn {
                        session,
                        turn,
                        call: ended_call,
                        attempt: ended_attempt,
                        ambiguous_operations,
                    },
                ))
            }
        }
    }
}
