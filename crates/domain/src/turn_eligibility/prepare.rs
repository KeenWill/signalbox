//! Turn scheduling prepare for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::activated_turn::{AcceptedInputTurnActivationIdentities, ActivatedAcceptedInputTurn};
use super::failure::{
    AcceptedInputEligibilityError, AcceptedInputEligibilityFailure, AcceptedInputTurnFailureError,
    AcceptedInputTurnFailureFailure,
};
use super::prepared_activation::{
    AcceptedInputTurnStartingEntries, PreparedAcceptedInputTurnActivation,
};
use super::projection::{AcceptedInputSchedulingProjection, AcceptedInputTurnSchedulingStatus};
use super::turn_failure::{
    AcceptedInputTurnFailureIdentities, FailedAcceptedInputTurn, PreparedAcceptedInputTurnFailure,
};
use crate::model_execution::reclassify_pending_steering_inputs;
use crate::{
    AcceptedInputStartingLineage, AcceptedInputTurnStart, ActiveTurnPhase, CurrentTurnAttempt,
    InitialSemanticTranscriptEntryPayload, ResolvedContextFrontierSnapshot,
    SemanticTranscriptEntry, SemanticTranscriptEntryRef, TurnDisposition,
    UnstoppedAttemptDisposition,
};

pub(super) fn prepare_active_turn_lost_failure(
    projection: AcceptedInputSchedulingProjection,
    identities: AcceptedInputTurnFailureIdentities,
) -> Result<PreparedAcceptedInputTurnFailure, AcceptedInputTurnFailureError> {
    let fail = |projection, failure| AcceptedInputTurnFailureError {
        projection: Box::new(projection),
        identities: identities.clone(),
        failure,
    };

    let Some(active) = projection.active_turn() else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::NoActiveTurn,
        ));
    };
    let active = active.clone();

    let reclassified = match projection.active_turn_execution() {
        Some(execution) => reclassify_pending_steering_inputs(
            execution.session(),
            execution.turn(),
            execution.pending_steering(),
            identities.pending_steering_reclassifications(),
            execution.configuration().effective(),
        ),
        None => reclassify_pending_steering_inputs(
            active.session,
            active.turn,
            &[],
            identities.pending_steering_reclassifications(),
            active.origin_configuration.effective(),
        ),
    };
    let Ok(reclassified_pending_steering) = reclassified else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::PendingSteeringReclassificationMismatch,
        ));
    };

    let failure_ref =
        SemanticTranscriptEntryRef::from_source(active.session, identities.failure_entry);
    if projection.semantic_entries.contains_key(&failure_ref) {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::FailureEntryIdentityAlreadyExists,
        ));
    }
    if projection
        .snapshots
        .contains_key(&identities.terminal_frontier)
    {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::TerminalFrontierIdentityAlreadyExists,
        ));
    }

    let current_attempt = match active.active_phase() {
        Some(ActiveTurnPhase::Running { current_attempt }) => current_attempt.clone(),
        Some(
            ActiveTurnPhase::AwaitingApproval { .. }
            | ActiveTurnPhase::AwaitingChild { .. }
            | ActiveTurnPhase::AwaitingRecoveryDecision { .. }
            | ActiveTurnPhase::AwaitingRunnerRecovery { .. },
        )
        | None => {
            return Err(fail(
                projection,
                AcceptedInputTurnFailureFailure::ActiveAttemptCannotEndLost,
            ));
        }
    };
    let Ok(ended_attempt) = current_attempt.end_without_stop(UnstoppedAttemptDisposition::Lost)
    else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::ActiveAttemptCannotEndLost,
        ));
    };

    let Some(start) = active.start() else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::ActiveStartMissing,
        ));
    };
    let failure_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.failure_entry,
        active.session,
        InitialSemanticTranscriptEntryPayload::TurnFailed { turn: active.turn },
    );
    let Some(starting_snapshot) = projection.snapshots.get(&start.frontier().snapshot()) else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::StartingSnapshotMissing,
        ));
    };
    let Ok(terminal_snapshot) = starting_snapshot.derive_appending_candidate(
        identities.terminal_frontier,
        vec![failure_entry.reference()],
    ) else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::TerminalFrontierCannotAppend,
        ));
    };
    let turn = FailedAcceptedInputTurn {
        session: active.session,
        turn: active.turn,
        accepted_input: active.accepted_input,
        order: active.order,
        start,
        ended_attempt,
        disposition: TurnDisposition::Failed,
        terminal_frontier: identities.terminal_frontier,
    };

    Ok(PreparedAcceptedInputTurnFailure {
        turn,
        failure_entry,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(super) fn prepare_earliest_queued_activation(
    projection: AcceptedInputSchedulingProjection,
    identities: AcceptedInputTurnActivationIdentities,
) -> Result<PreparedAcceptedInputTurnActivation, AcceptedInputEligibilityError> {
    let fail = |projection, failure| AcceptedInputEligibilityError {
        projection: Box::new(projection),
        identities,
        failure,
    };

    if projection
        .attempt_owners
        .contains_key(&identities.initial_attempt)
    {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists,
        ));
    }
    if let Some(active) = projection.active_turn() {
        let turn = active.turn();
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::ActiveTurnPresent { turn },
        ));
    }
    if let Some(call) = projection.active_compaction_call {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::ContextCompactionInProgress { call },
        ));
    }
    let Some(index) = projection
        .turns
        .iter()
        .position(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
    else {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::NoQueuedTurn,
        ));
    };

    let source_session = projection.session.id();
    let origin_ref =
        SemanticTranscriptEntryRef::from_source(source_session, identities.origin_entry);
    if projection.semantic_entries.contains_key(&origin_ref) {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::OriginEntryIdentityAlreadyExists,
        ));
    }
    if projection
        .snapshots
        .contains_key(&identities.starting_frontier)
    {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::StartingFrontierIdentityAlreadyExists,
        ));
    }

    let queued = &projection.turns[index];
    let preceding_non_accepted_terminal = projection
        .preceding_non_accepted_successors
        .get(&queued.turn())
        .and_then(|predecessor| {
            projection
                .preceding_non_accepted_terminals
                .get(predecessor)
                .map(|(snapshot, selected)| (*predecessor, snapshot, *selected))
        });
    let origin_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.origin_entry,
        source_session,
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: queued.accepted_input.id(),
        },
    );
    let selected = queued
        .origin_configuration
        .effective()
        .model()
        .selected_direct();
    let previous_selected = preceding_non_accepted_terminal
        .as_ref()
        .map(|(_, _, selected)| *selected)
        .or_else(|| {
            index.checked_sub(1).map(|predecessor| {
                projection.turns[predecessor]
                    .origin_configuration
                    .effective()
                    .model()
                    .selected_direct()
            })
        });
    let model_identity_entry = previous_selected
        .filter(|previous| *previous != selected)
        .map(|_| {
            let reference = SemanticTranscriptEntryRef::from_source(
                source_session,
                identities.model_identity_entry,
            );
            if identities.model_identity_entry == identities.origin_entry
                || projection.semantic_entries.contains_key(&reference)
            {
                return Err(fail(
                    projection.clone(),
                    AcceptedInputEligibilityFailure::ModelIdentityEntryIdentityAlreadyExists,
                ));
            }
            Ok(SemanticTranscriptEntry::from_validated_parts(
                identities.model_identity_entry,
                source_session,
                InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                    turn: queued.turn,
                    defaults_version: queued.origin_configuration.session_defaults_version(),
                    selected,
                },
            ))
        })
        .transpose()?;
    let starting_entries = match model_identity_entry {
        Some(model_identity_entry) => AcceptedInputTurnStartingEntries::ModelIdentityThenOrigin([
            model_identity_entry,
            origin_entry,
        ]),
        None => AcceptedInputTurnStartingEntries::Origin([origin_entry]),
    };
    let starting_references = starting_entries
        .as_slice()
        .iter()
        .map(SemanticTranscriptEntry::reference)
        .collect::<Vec<_>>();
    let (lineage, starting_snapshot) = if index == 0 && preceding_non_accepted_terminal.is_none() {
        let seed = projection
            .initial_seed_frontier
            .and_then(|frontier| projection.snapshots.get(&frontier));
        let compacted = projection
            .latest_compaction_result
            .and_then(|frontier| projection.snapshots.get(&frontier))
            .filter(|latest| seed.is_some_and(|seed| seed.is_semantic_prefix_of(latest)));
        let base = compacted.or(seed);
        let snapshot = if let Some(base) = base {
            match base.derive_appending_candidate(
                identities.starting_frontier,
                starting_references.clone(),
            ) {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    return Err(fail(
                        projection,
                        AcceptedInputEligibilityFailure::InternalOriginFrontierConstructionFailed,
                    ));
                }
            }
        } else {
            match ResolvedContextFrontierSnapshot::try_from_candidate(
                source_session,
                identities.starting_frontier,
                starting_references.clone(),
            ) {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    return Err(fail(
                        projection,
                        AcceptedInputEligibilityFailure::InternalOriginFrontierConstructionFailed,
                    ));
                }
            }
        };
        (AcceptedInputStartingLineage::FirstInSession, snapshot)
    } else {
        let (predecessor_turn, terminal_frontier) =
            if let Some((predecessor_turn, terminal_frontier, _)) = preceding_non_accepted_terminal
            {
                (predecessor_turn, terminal_frontier)
            } else if let Some(predecessor_index) = index.checked_sub(1) {
                let predecessor = &projection.turns[predecessor_index];
                let predecessor_turn = predecessor.turn;
                let Some(terminal_frontier) = predecessor.terminal_frontier() else {
                    return Err(fail(
                    projection,
                    AcceptedInputEligibilityFailure::InternalPredecessorTerminalFrontierMissing {
                        predecessor: predecessor_turn,
                    },
                ));
                };
                (predecessor_turn, terminal_frontier)
            } else {
                return Err(fail(
                    projection,
                    AcceptedInputEligibilityFailure::InternalStartingFrontierDerivationFailed,
                ));
            };
        let compacted = projection
            .latest_compaction_result
            .and_then(|frontier| projection.snapshots.get(&frontier))
            .filter(|latest| terminal_frontier.is_semantic_prefix_of(latest));
        let base = compacted.unwrap_or(terminal_frontier);
        let snapshot = match base
            .derive_appending_candidate(identities.starting_frontier, starting_references)
        {
            Ok(snapshot) => snapshot,
            Err(_) => {
                return Err(fail(
                    projection,
                    AcceptedInputEligibilityFailure::InternalStartingFrontierDerivationFailed,
                ));
            }
        };
        (
            AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor_turn,
            },
            snapshot,
        )
    };
    let start =
        AcceptedInputTurnStart::from_validated_eligibility(lineage, starting_snapshot.frontier());
    let turn = ActivatedAcceptedInputTurn {
        session: source_session,
        turn: queued.turn,
        accepted_input: queued.accepted_input.clone(),
        order: queued.order,
        configuration: queued.origin_configuration.clone(),
        configuration_provenance: queued.configuration_provenance.clone(),
        start,
        phase: ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(identities.initial_attempt),
        },
        pending_steering: Box::new([]),
        consumed_steering: Box::new([]),
    };

    Ok(PreparedAcceptedInputTurnActivation {
        turn,
        starting_entries,
        starting_snapshot,
    })
}
