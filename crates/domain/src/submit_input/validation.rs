//! Submit-input origin and rejection validation for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::SubmitInput;
use super::SubmitInputAppliedResult;
use super::SubmitInputResult;
use super::reconstituted::SubmitInputReconstitutionFailure;
use super::reconstitution_input::SubmitInputTurnOriginReconstitutionInput;
use super::reconstitution_input::TurnOriginProvenance;
use crate::AcceptedInputDisposition;
use crate::AcceptedInputId;
use crate::AcceptedInputQueueOrder;
use crate::AppliedInterruptCommandResult;
use crate::AppliedInterruptState;
use crate::DeliveryRequest;
use crate::DurableCommandId;
use crate::FrozenModelSelection;
use crate::GoalTurnSource;
use crate::ModelChangeAdjustment;
use crate::ModelSelectionRequest;
use crate::ModelSettingsOverlay;
use crate::OriginConfiguration;
use crate::PerInputConfigurationChoices;
use crate::ReconciliationReason;
use crate::SessionConfigurationDefaults;
use crate::SessionConfigurationDefaultsVersion;
use crate::SessionId;
use crate::SessionInputPosition;
use crate::TurnDisposition;
use crate::TurnId;
use crate::UserContent;
use crate::ValidatedModelSettings;
use crate::VersionedSessionConfigurationDefaults;
use std::collections::HashSet;

pub(super) fn validate_existing_interrupt(
    command: &SubmitInput,
    active_turn: TurnId,
    interrupt: AppliedInterruptCommandResult,
    recorded_command: Option<DurableCommandId>,
) -> Result<(), SubmitInputReconstitutionFailure> {
    if interrupt.session() != command.session
        || interrupt.proof().predecessor() != active_turn
        || interrupt.proof().command() == command.command_id
        || recorded_command.is_some_and(|recorded| recorded != interrupt.proof().command())
    {
        return Err(SubmitInputReconstitutionFailure::ExistingInterruptMismatch);
    }
    Ok(())
}

pub(super) struct StoredOriginConfigurationReconstitutionFacts {
    pub(super) defaults_session: SessionId,
    pub(super) defaults_version: SessionConfigurationDefaultsVersion,
    pub(super) defaults: SessionConfigurationDefaults,
    pub(super) stored_requested_model: ModelSelectionRequest,
    pub(super) stored_frozen_model: FrozenModelSelection,
    pub(super) stored_model_settings: Option<ValidatedModelSettings>,
    pub(super) stored_model_settings_adjustments: Vec<ModelChangeAdjustment>,
}

pub(super) fn reconstruct_origin_configuration(
    command: &SubmitInput,
    facts: StoredOriginConfigurationReconstitutionFacts,
) -> Result<OriginConfiguration, SubmitInputReconstitutionFailure> {
    let StoredOriginConfigurationReconstitutionFacts {
        defaults_session,
        defaults_version,
        defaults,
        stored_requested_model,
        stored_frozen_model,
        stored_model_settings,
        stored_model_settings_adjustments,
    } = facts;
    let Some(configuration) = explicit_origin_configuration(command.delivery) else {
        return Err(SubmitInputReconstitutionFailure::AppliedDeliveryIsNotTurnOrigin);
    };
    if defaults_session != command.session {
        return Err(SubmitInputReconstitutionFailure::DefaultsSessionMismatch);
    }
    if defaults_version != configuration.expected_session_defaults_version() {
        return Err(SubmitInputReconstitutionFailure::DefaultsVersionMismatch);
    }

    let versioned = VersionedSessionConfigurationDefaults::reconstitute(defaults_version, defaults);
    let checked = versioned
        .derive_request_with_model_settings(
            defaults_version,
            configuration.model(),
            configuration.model_settings(),
        )
        .map_err(|_| SubmitInputReconstitutionFailure::DefaultsVersionMismatch)?;
    if checked.request().model() != stored_requested_model {
        return Err(SubmitInputReconstitutionFailure::RequestedModelMismatch);
    }
    let selected_direct = stored_frozen_model.selected_direct();
    let legacy_settings_are_safe = checked.request().per_call_model_settings()
        == ModelSettingsOverlay::inherit_all()
        && !checked
            .request()
            .model_settings()
            .validated_for()
            .is_some_and(|validated| validated != selected_direct);

    match stored_model_settings {
        Some(stored_model_settings) => OriginConfiguration::reconstitute_with_model_settings(
            checked,
            stored_frozen_model,
            stored_model_settings,
            stored_model_settings_adjustments,
        )
        .ok_or(SubmitInputReconstitutionFailure::FrozenModelMismatch),
        None if stored_model_settings_adjustments.is_empty() && legacy_settings_are_safe => {
            let frozen = OriginConfiguration::freeze(checked, |alias| match stored_frozen_model {
                FrozenModelSelection::FrozenAlias {
                    alias: stored_alias,
                    definition,
                } if stored_alias == alias => Some(definition),
                FrozenModelSelection::Direct(_) | FrozenModelSelection::FrozenAlias { .. } => None,
            })
            .map_err(|_| SubmitInputReconstitutionFailure::FrozenModelMismatch)?;
            (frozen.effective().model() == &stored_frozen_model)
                .then_some(frozen)
                .ok_or(SubmitInputReconstitutionFailure::FrozenModelMismatch)
        }
        None => Err(SubmitInputReconstitutionFailure::FrozenModelMismatch),
    }
}

fn explicit_origin_configuration(
    delivery: DeliveryRequest,
) -> Option<PerInputConfigurationChoices> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => Some(configuration),
        DeliveryRequest::NextSafePoint { .. } => None,
    }
}

pub(super) fn rejection_configuration(
    delivery: DeliveryRequest,
) -> Result<(PerInputConfigurationChoices, Option<TurnId>), SubmitInputReconstitutionFailure> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration } => Ok((configuration, None)),
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            configuration,
        }
        | DeliveryRequest::Interrupt {
            expected_active_turn,
            configuration,
            ..
        } => Ok((configuration, Some(expected_active_turn))),
        DeliveryRequest::NextSafePoint { .. } => {
            Err(SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration)
        }
    }
}

pub(super) fn position_exhaustion_origin(
    delivery: DeliveryRequest,
) -> Result<Option<TurnId>, SubmitInputReconstitutionFailure> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { .. } => Ok(None),
        DeliveryRequest::NextSafePoint {
            expected_active_turn,
        }
        | DeliveryRequest::Interrupt {
            expected_active_turn,
            ..
        }
        | DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            ..
        } => Ok(Some(expected_active_turn)),
    }
}

pub(super) struct ValidatedTurnOrigin {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) acceptance_position: SessionInputPosition,
    pub(super) accepted_input: AcceptedInputId,
    pub(super) content: UserContent,
    pub(super) accepted_inputs: HashSet<AcceptedInputId>,
    pub(super) command_ids: HashSet<DurableCommandId>,
    pub(super) turns: HashSet<TurnId>,
}

fn goal_turn_source_references_turn(source: GoalTurnSource, turn: TurnId) -> bool {
    match source {
        GoalTurnSource::UserEvent(_) => false,
        GoalTurnSource::PredecessorTurn(predecessor) => predecessor == turn,
    }
}

pub(super) fn validate_turn_origin_reconstitution_input(
    input: &SubmitInputTurnOriginReconstitutionInput,
) -> Option<ValidatedTurnOrigin> {
    struct ValidatedOriginPosition {
        session: SessionId,
        turn: TurnId,
        acceptance_position: SessionInputPosition,
        accepted_input: AcceptedInputId,
        content: UserContent,
    }

    let mut validated: Option<ValidatedOriginPosition> = None;
    let mut accepted_inputs = HashSet::with_capacity(input.chain.len());
    let mut command_ids = HashSet::with_capacity(input.chain.len());
    let mut turns = HashSet::with_capacity(input.chain.len());

    for facts in &input.chain {
        let receipt = match &facts.provenance {
            TurnOriginProvenance::Submit(receipt) => receipt,
            TurnOriginProvenance::Goal(goal) => {
                if validated.is_some()
                    || facts.source_terminal.is_some()
                    || !accepted_inputs.insert(goal.accepted_input)
                    || !turns.insert(goal.turn)
                    || facts.lifecycle.id() != goal.accepted_input
                    || facts.lifecycle.disposition()
                        != &AcceptedInputDisposition::OriginOf(goal.turn)
                    || facts.queue_accepted_input != goal.accepted_input
                    || facts.queue_session != goal.session
                    || facts.queue_turn != goal.turn
                    || facts.queue_order
                        != AcceptedInputQueueOrder::ordinary(goal.acceptance_position)
                    || goal_turn_source_references_turn(goal.source, goal.turn)
                {
                    return None;
                }
                validated = Some(ValidatedOriginPosition {
                    session: goal.session,
                    turn: goal.turn,
                    acceptance_position: goal.acceptance_position,
                    accepted_input: goal.accepted_input,
                    content: goal.content.clone(),
                });
                continue;
            }
        };
        let SubmitInputResult::Applied(applied) = receipt.result() else {
            return None;
        };
        if !accepted_inputs.insert(applied.accepted_input())
            || !command_ids.insert(receipt.command().command_id())
        {
            return None;
        }
        let (turn, expected_queue_order) = match (
            applied,
            facts.lifecycle.disposition(),
            &facts.source_terminal,
            validated.as_ref(),
        ) {
            (
                SubmitInputAppliedResult::TurnOrigin(origin),
                AcceptedInputDisposition::OriginOf(turn),
                None,
                None,
            ) if *turn == origin.turn() => (*turn, origin.queue_order()),
            (
                SubmitInputAppliedResult::PendingSteering(pending),
                AcceptedInputDisposition::ReclassifiedAsTurnOrigin { turn, .. },
                Some(source_terminal),
                Some(source_origin),
            ) if *turn != pending.binding().source_turn() => {
                if source_origin.session != applied.session()
                    || source_origin.turn != pending.binding().source_turn()
                    || source_terminal.turn != source_origin.turn
                    || source_origin.acceptance_position >= applied.acceptance_position()
                    || !terminal_disposition_matches_turn(
                        &source_terminal.disposition,
                        source_origin.turn,
                    )
                {
                    return None;
                }
                if let Some(command) = terminal_disposition_command(&source_terminal.disposition)
                    && !command_ids.insert(command)
                {
                    return None;
                }
                (
                    *turn,
                    AcceptedInputQueueOrder::ordinary(applied.acceptance_position()),
                )
            }
            _ => return None,
        };
        if facts.lifecycle.id() != applied.accepted_input()
            || facts.queue_accepted_input != applied.accepted_input()
            || facts.queue_session != applied.session()
            || facts.queue_turn != turn
            || facts.queue_order != expected_queue_order
            || !turns.insert(turn)
        {
            return None;
        }

        validated = Some(ValidatedOriginPosition {
            session: applied.session(),
            turn,
            acceptance_position: applied.acceptance_position(),
            accepted_input: applied.accepted_input(),
            content: receipt.command().content().clone(),
        });
    }

    let validated = validated?;
    Some(ValidatedTurnOrigin {
        session: validated.session,
        turn: validated.turn,
        acceptance_position: validated.acceptance_position,
        accepted_input: validated.accepted_input,
        content: validated.content,
        accepted_inputs,
        command_ids,
        turns,
    })
}

fn terminal_disposition_command(disposition: &TurnDisposition) -> Option<DurableCommandId> {
    match disposition {
        TurnDisposition::Completed
        | TurnDisposition::Refused
        | TurnDisposition::Failed
        | TurnDisposition::Retired => None,
        TurnDisposition::Cancelled { cause } => Some(cause.command()),
        TurnDisposition::ReconciliationRequired { marker } => match marker.reason() {
            ReconciliationReason::UserChoseReconciliation { decision } => {
                Some(decision.decision_command())
            }
            ReconciliationReason::InterruptRequiresReconciliation { interrupt } => {
                Some(interrupt.command())
            }
            ReconciliationReason::FatalMismatchRequiresReconciliation { causes } => {
                match causes.interrupt() {
                    AppliedInterruptState::NoAppliedInterrupt => None,
                    AppliedInterruptState::Applied { proof } => Some(proof.command()),
                }
            }
            ReconciliationReason::AutomaticRecovery { .. } => None,
        },
    }
}

fn terminal_disposition_matches_turn(disposition: &TurnDisposition, turn: TurnId) -> bool {
    match disposition {
        TurnDisposition::Completed | TurnDisposition::Refused | TurnDisposition::Failed => true,
        // A retired turn never activated, so it was never a steering source.
        TurnDisposition::Retired => false,
        TurnDisposition::Cancelled { cause } => cause.predecessor() == turn,
        TurnDisposition::ReconciliationRequired { marker } => match marker.reason() {
            ReconciliationReason::UserChoseReconciliation { decision } => decision.turn() == turn,
            ReconciliationReason::InterruptRequiresReconciliation { interrupt } => {
                interrupt.predecessor() == turn
            }
            ReconciliationReason::FatalMismatchRequiresReconciliation { causes } => {
                match causes.interrupt() {
                    AppliedInterruptState::NoAppliedInterrupt => true,
                    AppliedInterruptState::Applied { proof } => proof.predecessor() == turn,
                }
            }
            ReconciliationReason::AutomaticRecovery { .. } => true,
        },
    }
}

pub(super) fn validate_rejection_active_turn_origin(
    command: &SubmitInput,
    expected_turn: Option<TurnId>,
    origin: Option<&SubmitInputTurnOriginReconstitutionInput>,
) -> Result<(), SubmitInputReconstitutionFailure> {
    match (expected_turn, origin) {
        (None, None) => Ok(()),
        (Some(expected_turn), Some(origin)) => {
            let Some(result) = validate_turn_origin_reconstitution_input(origin) else {
                return Err(SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch);
            };
            if result.session != command.session || result.turn != expected_turn {
                return Err(SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch);
            }
            if result.command_ids.contains(&command.command_id) {
                return Err(
                    SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused,
                );
            }
            Ok(())
        }
        (None, Some(_)) | (Some(_), None) => {
            Err(SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch)
        }
    }
}

pub(super) fn expected_active_turn(delivery: DeliveryRequest) -> Option<TurnId> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { .. } => None,
        DeliveryRequest::Interrupt {
            expected_active_turn,
            ..
        }
        | DeliveryRequest::NextSafePoint {
            expected_active_turn,
        }
        | DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            ..
        } => Some(expected_active_turn),
    }
}
