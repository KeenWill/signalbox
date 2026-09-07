//! Model-call interrupt application for `docs/spec/model-call-execution.md`.

use super::{
    AmbiguousModelCallTurnIdentities, CancellationFrontierSource, CancelledModelCallTurn,
    CancelledModelCallTurnIdentities, ModelCallClosureError, ModelCallTurnScope,
    PendingSteeringReclassificationIdentity, ReclassifiedPendingSteeringTurn,
    ReconciliationRequiredModelCallTurn, ReconciliationRequiredToolTurn, close_cancelled_turn,
};
use crate::{
    AcceptedInputDisposition, AcceptedInputQueueOrder, ActivatedTurn, ActiveTurnPhase,
    AppliedInterruptCommandResult, AttemptEnd, AwaitingToolRecovery, CancellationStopDisposition,
    EffectiveConfiguration, EndedModelCall, EndedToolAttempt, EndedTurnAttempt,
    ModelCallDisposition, NonEmptyIssuedOperationRefs, PreparedToolResultProjection,
    ReconciliationMarker, ResolvedContextFrontierSnapshot, SemanticTranscriptEntryPayload,
    SessionId, SteeringReclassificationReason, TurnDisposition, TurnId,
    UnstoppedAttemptDisposition,
};
use std::collections::BTreeSet;

pub(super) fn reclassify_pending_steering(
    active_turn: &ActivatedTurn,
    identities: &[PendingSteeringReclassificationIdentity],
) -> Result<Box<[ReclassifiedPendingSteeringTurn]>, ModelCallClosureError> {
    reclassify_pending_steering_inputs(
        active_turn.session(),
        active_turn.turn(),
        active_turn.pending_steering(),
        identities,
        active_turn.configuration().effective(),
    )
}

pub(crate) fn reclassify_pending_steering_inputs(
    session: SessionId,
    source_turn: TurnId,
    pending: &[crate::PendingSteeringInput],
    identities: &[PendingSteeringReclassificationIdentity],
    effective_configuration: &EffectiveConfiguration,
) -> Result<Box<[ReclassifiedPendingSteeringTurn]>, ModelCallClosureError> {
    if pending.len() != identities.len() {
        return Err(ModelCallClosureError::PendingSteeringReclassificationMismatch);
    }

    let mut turns = BTreeSet::new();
    let mut reclassified = Vec::with_capacity(pending.len());
    for (pending, identity) in pending.iter().zip(identities) {
        let AcceptedInputDisposition::PendingSteering { binding } =
            pending.lifecycle().disposition()
        else {
            return Err(ModelCallClosureError::PendingSteeringReclassificationMismatch);
        };
        if pending.accepted_input() != identity.accepted_input
            || binding.source_turn() != source_turn
            || identity.turn == source_turn
            || !turns.insert(identity.turn)
        {
            return Err(ModelCallClosureError::PendingSteeringReclassificationMismatch);
        }
        let accepted_input = pending
            .lifecycle()
            .clone()
            .reclassify_as_turn_origin(
                identity.turn,
                SteeringReclassificationReason::NoSafePointBeforeTerminal,
            )
            .map_err(|_| ModelCallClosureError::PendingSteeringReclassificationMismatch)?;
        reclassified.push(ReclassifiedPendingSteeringTurn {
            session,
            source_turn,
            accepted_input,
            turn: identity.turn,
            order: AcceptedInputQueueOrder::ordinary(pending.acceptance_position()),
            binding: *binding,
            effective_configuration: effective_configuration.clone(),
        });
    }
    Ok(reclassified.into_boxed_slice())
}

pub(crate) fn apply_interrupt_to_recovery_wait(
    active_turn: ActivatedTurn,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    source_snapshot: ResolvedContextFrontierSnapshot,
    interrupt: AppliedInterruptCommandResult,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredModelCallTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        applied_interrupt: None,
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    if interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || ambiguous_operations.operation_count() != 1
        || !ambiguous_operations.contains(crate::IssuedOperationRef::ModelCall(call.id()))
        || call.turn() != active_turn.turn()
        || call.attempt() != attempt.id()
        || call.disposition() != ModelCallDisposition::Ambiguous
        || call.frontier() != source_snapshot.frontier()
        || source_snapshot.frontier().owning_session() != active_turn.session()
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let terminal_snapshot = source_snapshot
        .derive_appending_candidate(identities.terminal_frontier, Vec::new())
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    if !matches!(
        attempt.end(),
        AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost,
        }
    ) {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    }
    let marker =
        ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations.clone(), proof);
    Ok(ReconciliationRequiredModelCallTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        call,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_automatic_reconciliation(
    active_turn: ActivatedTurn,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    source_snapshot: ResolvedContextFrontierSnapshot,
    recovery_attempt: std::num::NonZeroU32,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredModelCallTurn, ModelCallClosureError> {
    let ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        applied_interrupt,
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    if ambiguous_operations.operation_count() != 1
        || !ambiguous_operations.contains(crate::IssuedOperationRef::ModelCall(call.id()))
        || call.turn() != active_turn.turn()
        || call.attempt() != attempt.id()
        || call.disposition() != ModelCallDisposition::Ambiguous
        || call.frontier() != source_snapshot.frontier()
        || source_snapshot.frontier().owning_session() != active_turn.session()
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let terminal_snapshot = source_snapshot
        .derive_appending_candidate(identities.terminal_frontier, Vec::new())
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    if !matches!(
        attempt.end(),
        AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost,
        }
    ) {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    }
    let marker = match applied_interrupt {
        Some(proof) => {
            ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations.clone(), *proof)
        }
        None => ReconciliationMarker::from_automatic_recovery(
            ambiguous_operations.clone(),
            recovery_attempt,
        ),
    };
    Ok(ReconciliationRequiredModelCallTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        call,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_interrupt_to_runner_recovery_wait(
    active_turn: ActivatedTurn,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    source_snapshot: ResolvedContextFrontierSnapshot,
    result_projection: Option<PreparedToolResultProjection>,
    interrupt: AppliedInterruptCommandResult,
    identities: CancelledModelCallTurnIdentities,
) -> Result<CancelledModelCallTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    if !matches!(
        active_turn.phase(),
        ActiveTurnPhase::AwaitingRunnerRecovery {
            optional_tool_attempt: None,
            ..
        }
    ) || interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || starting_snapshot.frontier() != active_turn.start().frontier()
        || source_snapshot.frontier().owning_session() != active_turn.session()
        || !starting_snapshot.is_semantic_prefix_of(&source_snapshot)
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let tool_result_entries = match result_projection {
        Some(projection)
            if projection.turn() == active_turn.turn()
                && projection.source_frontier() == source_snapshot.frontier().snapshot()
                && projection.snapshot().frontier().owning_session() == active_turn.session()
                && source_snapshot.is_semantic_prefix_of(projection.snapshot()) =>
        {
            projection.into_parts().0
        }
        None => Vec::new().into_boxed_slice(),
        Some(_) => return Err(ModelCallClosureError::InterruptCorrelationMismatch),
    };
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let mut cancelled = close_cancelled_turn(
        ModelCallTurnScope {
            session: active_turn.session(),
            turn: active_turn.turn(),
        },
        None,
        None,
        CancellationFrontierSource::new(source_snapshot, &tool_result_entries),
        proof,
        identities,
        reclassified_pending_steering,
    )?;
    cancelled.tool_result_entries = tool_result_entries;
    Ok(cancelled)
}

pub(crate) fn apply_interrupt_to_runner_tool_recovery_wait(
    active_turn: ActivatedTurn,
    wait: AwaitingToolRecovery,
    tool_attempt: EndedToolAttempt,
    yielded_attempt: crate::TurnAttemptId,
    result_projection: PreparedToolResultProjection,
    interrupt: AppliedInterruptCommandResult,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let ActiveTurnPhase::AwaitingRunnerRecovery {
        optional_tool_attempt: Some(interrupted_tool_attempt),
        ..
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let attempt = EndedTurnAttempt::reconstitute_yielded(yielded_attempt);
    if interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || *interrupted_tool_attempt != wait.attempt()
        || wait.session() != active_turn.session()
        || wait.turn() != active_turn.turn()
        || wait.producing_call() != result_projection.producing_call()
        || wait.issuing_attempt() != yielded_attempt
        || wait.attempt() != tool_attempt.attempt()
        || result_projection.turn() != active_turn.turn()
        || wait.yielded_frontier() != result_projection.source_frontier()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.snapshot().frontier().snapshot() != identities.terminal_frontier
        || !result_projection.entries().iter().any(|entry| {
            entry.payload()
                == &SemanticTranscriptEntryPayload::ToolClosed {
                    request: tool_attempt.request(),
                }
        })
        || tool_attempt.session() != active_turn.session()
        || tool_attempt.turn() != active_turn.turn()
        || tool_attempt.issuing_attempt() != yielded_attempt
        || tool_attempt.end() != &crate::ToolAttemptEnd::Ambiguous
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let ambiguous_operations =
        NonEmptyIssuedOperationRefs::try_from_operations([crate::IssuedOperationRef::ToolAttempt(
            wait.attempt(),
        )])
        .map_err(|_| ModelCallClosureError::InterruptCorrelationMismatch)?;
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, terminal_snapshot) = result_projection.into_parts();
    let marker = ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations, proof);
    Ok(ReconciliationRequiredToolTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        tool_attempt,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        tool_result_entries,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_interrupt_to_retryable_runner_tool_recovery_wait(
    active_turn: ActivatedTurn,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    batch: crate::ToolBatch,
    result_projection: PreparedToolResultProjection,
    interrupt: AppliedInterruptCommandResult,
    identities: CancelledModelCallTurnIdentities,
) -> Result<CancelledModelCallTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let ActiveTurnPhase::AwaitingRunnerRecovery {
        optional_tool_attempt: Some(interrupted_tool_attempt),
        ..
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let crate::ToolBatchPhase::Executing { turn_attempt } = batch.phase() else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let stopped_attempt =
        batch
            .requests()
            .iter()
            .find_map(|request| match batch.attempt(request.id()) {
                Some(crate::ReconstitutedToolAttempt::Ended(attempt))
                    if attempt.attempt() == *interrupted_tool_attempt =>
                {
                    Some(attempt)
                }
                Some(crate::ReconstitutedToolAttempt::Current(_))
                | Some(crate::ReconstitutedToolAttempt::Ended(_))
                | None => None,
            });
    let Some(stopped_attempt) = stopped_attempt else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    if batch.session() != active_turn.session()
        || batch.turn() != active_turn.turn()
        || stopped_attempt.session() != active_turn.session()
        || stopped_attempt.turn() != active_turn.turn()
        || stopped_attempt.issuing_attempt() != turn_attempt
        || !matches!(
            stopped_attempt.end(),
            crate::ToolAttemptEnd::KnownFailed { error }
                if error.kind() == crate::ToolExecutionErrorKind::CrashLost
                    && error.detail().is_none()
        )
        || interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || starting_snapshot.frontier() != active_turn.start().frontier()
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let source_snapshot = batch.yielded_snapshot().clone();
    if !starting_snapshot.is_semantic_prefix_of(&source_snapshot)
        || result_projection.turn() != active_turn.turn()
        || result_projection.producing_call() != batch.producing_call()
        || result_projection.source_frontier() != source_snapshot.frontier().snapshot()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.snapshot().frontier().snapshot() != identities.terminal_frontier
        || !source_snapshot.is_semantic_prefix_of(result_projection.snapshot())
        || !result_projection.entries().iter().any(|entry| {
            entry.payload()
                == &SemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: *interrupted_tool_attempt,
                }
        })
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, _) = result_projection.into_parts();
    let mut cancelled = close_cancelled_turn(
        ModelCallTurnScope {
            session: active_turn.session(),
            turn: active_turn.turn(),
        },
        None,
        None,
        CancellationFrontierSource::new(source_snapshot, &tool_result_entries),
        proof,
        identities,
        reclassified_pending_steering,
    )?;
    cancelled.tool_result_entries = tool_result_entries;
    Ok(cancelled)
}

pub(crate) fn apply_interrupt_to_tool_recovery_wait(
    active_turn: ActivatedTurn,
    wait: AwaitingToolRecovery,
    tool_attempt: EndedToolAttempt,
    attempt: EndedTurnAttempt,
    result_projection: PreparedToolResultProjection,
    interrupt: AppliedInterruptCommandResult,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        applied_interrupt,
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let attempt_end_matches = match attempt.end() {
        AttemptEnd::WithoutStop { disposition } => {
            applied_interrupt.is_none()
                && matches!(
                    disposition,
                    UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost
                )
        }
        AttemptEnd::AfterCancellation { cause, disposition } => {
            *cause == proof
                && applied_interrupt == &Some(proof)
                && matches!(
                    disposition,
                    CancellationStopDisposition::Ambiguous | CancellationStopDisposition::Lost
                )
        }
        AttemptEnd::AfterFatalMismatch { .. } => false,
    };
    if interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || ambiguous_operations.operation_count() != 1
        || !ambiguous_operations.contains(crate::IssuedOperationRef::ToolAttempt(wait.attempt()))
        || wait.session() != active_turn.session()
        || wait.turn() != active_turn.turn()
        || wait.producing_call() != result_projection.producing_call()
        || wait.issuing_attempt() != attempt.id()
        || wait.attempt() != tool_attempt.attempt()
        || result_projection.turn() != active_turn.turn()
        || wait.yielded_frontier() != result_projection.source_frontier()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.snapshot().frontier().snapshot() != identities.terminal_frontier
        || !result_projection.entries().iter().any(|entry| {
            entry.payload()
                == &SemanticTranscriptEntryPayload::ToolClosed {
                    request: tool_attempt.request(),
                }
        })
        || tool_attempt.session() != active_turn.session()
        || tool_attempt.turn() != active_turn.turn()
        || tool_attempt.issuing_attempt() != attempt.id()
        || tool_attempt.end() != &crate::ToolAttemptEnd::Ambiguous
        || !attempt_end_matches
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, terminal_snapshot) = result_projection.into_parts();
    let marker =
        ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations.clone(), proof);
    Ok(ReconciliationRequiredToolTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        tool_attempt,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        tool_result_entries,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_automatic_tool_reconciliation(
    active_turn: ActivatedTurn,
    wait: AwaitingToolRecovery,
    tool_attempt: EndedToolAttempt,
    attempt: EndedTurnAttempt,
    result_projection: PreparedToolResultProjection,
    recovery_attempt: std::num::NonZeroU32,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError> {
    let ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        applied_interrupt,
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let attempt_end_matches = match attempt.end() {
        AttemptEnd::WithoutStop { disposition } => {
            applied_interrupt.is_none()
                && matches!(
                    disposition,
                    UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost
                )
        }
        AttemptEnd::AfterCancellation { cause, disposition } => {
            applied_interrupt == &Some(*cause)
                && matches!(
                    disposition,
                    CancellationStopDisposition::Ambiguous | CancellationStopDisposition::Lost
                )
        }
        AttemptEnd::AfterFatalMismatch { .. } => false,
    };
    if ambiguous_operations.operation_count() != 1
        || !ambiguous_operations.contains(crate::IssuedOperationRef::ToolAttempt(wait.attempt()))
        || wait.session() != active_turn.session()
        || wait.turn() != active_turn.turn()
        || wait.producing_call() != result_projection.producing_call()
        || wait.issuing_attempt() != attempt.id()
        || wait.attempt() != tool_attempt.attempt()
        || result_projection.turn() != active_turn.turn()
        || wait.yielded_frontier() != result_projection.source_frontier()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.snapshot().frontier().snapshot() != identities.terminal_frontier
        || !result_projection.entries().iter().any(|entry| {
            entry.payload()
                == &SemanticTranscriptEntryPayload::ToolClosed {
                    request: tool_attempt.request(),
                }
        })
        || tool_attempt.session() != active_turn.session()
        || tool_attempt.turn() != active_turn.turn()
        || tool_attempt.issuing_attempt() != attempt.id()
        || tool_attempt.end() != &crate::ToolAttemptEnd::Ambiguous
        || !attempt_end_matches
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, terminal_snapshot) = result_projection.into_parts();
    let marker = match applied_interrupt {
        Some(proof) => {
            ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations.clone(), *proof)
        }
        None => ReconciliationMarker::from_automatic_recovery(
            ambiguous_operations.clone(),
            recovery_attempt,
        ),
    };
    Ok(ReconciliationRequiredToolTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        tool_attempt,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        tool_result_entries,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_interrupt_to_executing_tool_batch(
    active_turn: ActivatedTurn,
    batch: crate::ToolBatch,
    result_projection: PreparedToolResultProjection,
    interrupt: AppliedInterruptCommandResult,
    identities: CancelledModelCallTurnIdentities,
) -> Result<CancelledModelCallTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let attempt = match (active_turn.phase(), batch.phase()) {
        (
            ActiveTurnPhase::Running { current_attempt },
            crate::ToolBatchPhase::Executing { turn_attempt },
        ) if turn_attempt == current_attempt.id() => Some(current_attempt.clone()),
        (
            ActiveTurnPhase::AwaitingChild { wait },
            crate::ToolBatchPhase::AwaitingChild {
                request,
                spawning_request,
                child,
            },
        ) if request == wait.awaiting_request()
            && spawning_request == wait.spawning_request()
            && child == wait.child() =>
        {
            None
        }
        (ActiveTurnPhase::Running { .. }, crate::ToolBatchPhase::Executing { .. }) => {
            return Err(ModelCallClosureError::InterruptCorrelationMismatch);
        }
        _ => return Err(ModelCallClosureError::AttemptStateMismatch),
    };
    if batch.session() != active_turn.session()
        || batch.turn() != active_turn.turn()
        || interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let source_snapshot = batch.yielded_snapshot().clone();
    if result_projection.turn() != active_turn.turn()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.source_frontier() != source_snapshot.frontier().snapshot()
        || !source_snapshot.is_semantic_prefix_of(result_projection.snapshot())
        || result_projection.snapshot().entry_count()
            != source_snapshot.entry_count() + result_projection.entries().len()
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, _result_snapshot) = result_projection.into_parts();
    let mut cancelled = close_cancelled_turn(
        ModelCallTurnScope {
            session: active_turn.session(),
            turn: active_turn.turn(),
        },
        attempt,
        None,
        CancellationFrontierSource::new(source_snapshot, &tool_result_entries),
        proof,
        identities,
        reclassified_pending_steering,
    )?;
    cancelled.tool_result_entries = tool_result_entries;
    Ok(cancelled)
}
