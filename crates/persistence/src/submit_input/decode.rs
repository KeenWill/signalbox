use super::load::{RelatedTurnEvidence, configured_model_settings};
use super::{
    APPLIED, REJECTED, SubmitInputCorruption, SubmitInputRepositoryError, decode_actor,
    decode_content, decode_dangerous_tool_auto_approval, decode_defaults, decode_defaults_version,
    decode_delivery, decode_frozen_model, decode_model_selection, decode_optional_defaults_version,
    decode_optional_position, decode_position, require_all_absent, require_spelling,
    require_supported_version, required,
};
use crate::command_registry::SUBMIT_INPUT_KIND;
use crate::mapping::{
    accepted_input_id_from_uuid, defaults_version_from_numeric, durable_command_id_from_uuid,
    durable_command_id_to_uuid, model_change_adjustments_from_json, model_settings_from_json,
    model_settings_overlay_from_json, positive_u64_from_numeric, session_id_from_uuid,
    turn_id_from_uuid,
};
use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputQueueOrder, Actor,
    AppliedInterruptCommandResult, DelegatedTurnSchedulingState, DelegationContent,
    DelegationOutcomeKind, DelegationOutcomeReason, DelegationProvenanceReconstitutionInput,
    DelegationWaitMode, DeliveryRequest, DescendantTerminationScope, DirectModelSelection,
    DurableCommandId, GoalGeneration, ModelAlias, NonAcceptedTurnPredecessorReconstitutionInput,
    OriginConfiguration, OriginConfigurationReconstitutionInput, ReconstitutedSubmitInput,
    SessionId, SubmitInput, SubmitInputAppliedPendingSteeringReconstitutionInput,
    SubmitInputAppliedResult, SubmitInputAppliedTurnOriginReconstitutionInput,
    SubmitInputReconstitutionInput,
    SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput,
    SubmitInputRejectedActiveTurnMismatchReconstitutionInput,
    SubmitInputRejectedActiveTurnPresentReconstitutionInput,
    SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput,
    SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput,
    SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput,
    SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput,
    SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput,
    SubmitInputRejectedNoActiveTurnReconstitutionInput,
    SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput,
    SubmitInputRejectedSessionNotFoundReconstitutionInput,
    SubmitInputRejectedUnknownModelAliasReconstitutionInput, SubmitInputResult,
    SubmitInputTurnOriginReconstitutionInput, TurnAttemptId, TurnId,
};
use sqlx::Row;
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use std::collections::BTreeMap;
use std::num::NonZeroU64;

pub(super) fn delegation_content(
    value: String,
    field: &'static str,
) -> Result<DelegationContent, SubmitInputCorruption> {
    DelegationContent::try_new(value).map_err(|_| SubmitInputCorruption::Inconsistent(field))
}

pub(super) fn decode_delegated_turn_scheduling_state(
    state_kind: &str,
    terminal_disposition: Option<&str>,
    runtime_terminal: bool,
) -> Result<DelegatedTurnSchedulingState, SubmitInputCorruption> {
    match (state_kind, terminal_disposition, runtime_terminal) {
        ("active", None, false) => Ok(DelegatedTurnSchedulingState::Active),
        ("queued" | "active", None, true) => Ok(DelegatedTurnSchedulingState::RuntimeTerminal),
        ("terminal", Some("completed"), _) => Ok(DelegatedTurnSchedulingState::TerminalCompleted),
        ("terminal", Some("refused"), _) => Ok(DelegatedTurnSchedulingState::TerminalRefused),
        ("terminal", Some("failed"), _) => Ok(DelegatedTurnSchedulingState::TerminalFailed),
        ("terminal", Some("cancelled"), _) => Ok(DelegatedTurnSchedulingState::TerminalCancelled),
        ("terminal", Some("reconciliation_required"), _) => {
            Ok(DelegatedTurnSchedulingState::TerminalReconciliationRequired)
        }
        _ => Err(SubmitInputCorruption::Inconsistent(
            "delegated turn scheduling state",
        )),
    }
}

pub(super) fn decode_delegation_wait_mode(
    value: &str,
) -> Result<DelegationWaitMode, SubmitInputCorruption> {
    match value {
        "foreground" => Ok(DelegationWaitMode::Foreground),
        "background" => Ok(DelegationWaitMode::Background),
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "delegation wait mode",
            value: value.to_owned(),
        }),
    }
}

pub(super) fn decode_delegation_outcome_kind(
    value: &str,
) -> Result<DelegationOutcomeKind, SubmitInputCorruption> {
    match value {
        "result_returned" => Ok(DelegationOutcomeKind::ResultReturned),
        "child_failed" => Ok(DelegationOutcomeKind::ChildFailed),
        "child_stopped" => Ok(DelegationOutcomeKind::ChildStopped),
        "child_cancelled" => Ok(DelegationOutcomeKind::ChildCancelled),
        "already_terminal" => Ok(DelegationOutcomeKind::AlreadyTerminal),
        "continue_running" => Ok(DelegationOutcomeKind::ContinueRunning),
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "delegation outcome",
            value: value.to_owned(),
        }),
    }
}

pub(super) fn decode_delegation_outcome_reason(
    value: &str,
) -> Result<DelegationOutcomeReason, SubmitInputCorruption> {
    match value {
        "child_completed" => Ok(DelegationOutcomeReason::ChildCompleted),
        "child_execution_failed" => Ok(DelegationOutcomeReason::ChildExecutionFailed),
        "child_result_unavailable" => Ok(DelegationOutcomeReason::ChildResultUnavailable),
        "child_cancelled" => Ok(DelegationOutcomeReason::ChildCancelled),
        "parent_stopped_parent_and_descendants" => Ok(DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }),
        "parent_cancelled_parent_and_descendants" => Ok(DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }),
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "delegation outcome reason",
            value: value.to_owned(),
        }),
    }
}

pub(super) fn decode_delegation_provenance(
    kind: &str,
    session: Uuid,
    turn: Option<Uuid>,
    generation: Option<Decimal>,
    command: Option<Uuid>,
) -> Result<DelegationProvenanceReconstitutionInput, SubmitInputCorruption> {
    match (kind, turn, generation, command) {
        ("child_turn", Some(turn), None, None) => {
            Ok(DelegationProvenanceReconstitutionInput::ChildTurn {
                session: session_id_from_uuid(session),
                turn: turn_id_from_uuid(turn),
            })
        }
        ("parent_turn_command", Some(turn), None, Some(command)) => {
            Ok(DelegationProvenanceReconstitutionInput::ParentTurnCommand {
                session: session_id_from_uuid(session),
                turn: turn_id_from_uuid(turn),
                command: durable_command_id_from_uuid(command).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("delegation provenance command")
                })?,
            })
        }
        ("parent_goal_command", None, Some(generation), Some(command)) => {
            let generation = positive_u64_from_numeric(generation)
                .ok()
                .and_then(NonZeroU64::new)
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "delegation provenance generation",
                ))?;
            Ok(DelegationProvenanceReconstitutionInput::ParentGoalCommand {
                session: session_id_from_uuid(session),
                generation: GoalGeneration::new(generation),
                command: durable_command_id_from_uuid(command).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("delegation provenance command")
                })?,
            })
        }
        ("parent_lifecycle_command", None, None, Some(command)) => Ok(
            DelegationProvenanceReconstitutionInput::ParentLifecycleCommand {
                session: session_id_from_uuid(session),
                command: durable_command_id_from_uuid(command).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("delegation provenance command")
                })?,
            },
        ),
        _ => Err(SubmitInputCorruption::Inconsistent(
            "delegation result provenance",
        )),
    }
}

pub(super) fn map_imported_scheduling_error(
    error: crate::create_session_from_imported_frontier::ImportedSessionRepositoryError,
) -> SubmitInputRepositoryError {
    use crate::create_session_from_imported_frontier::ImportedSessionRepositoryError;

    match error {
        ImportedSessionRepositoryError::Database(error) => {
            SubmitInputRepositoryError::Database(error)
        }
        ImportedSessionRepositoryError::ImportedConversation(
            crate::conversation_import::ImportedConversationRepositoryError::Database(error),
        ) => SubmitInputRepositoryError::Database(error),
        ImportedSessionRepositoryError::Corruption(_)
        | ImportedSessionRepositoryError::CommitAmbiguous(_)
        | ImportedSessionRepositoryError::DifferentCommandKind { .. }
        | ImportedSessionRepositoryError::Preparation(_)
        | ImportedSessionRepositoryError::IdentityCollision(_)
        | ImportedSessionRepositoryError::ImportedConversation(_) => {
            SubmitInputCorruption::Inconsistent("complete imported scheduling projection").into()
        }
    }
}

pub(super) fn require_current_attempt_row(
    row: &PgRow,
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
) -> Result<(), SubmitInputRepositoryError> {
    let stored_attempt: Option<Uuid> = row.try_get("turn_attempt_id")?;
    let stored_turn: Option<Uuid> = row.try_get("attempt_turn_id")?;
    let stored_session: Option<Uuid> = row.try_get("attempt_session_id")?;
    if stored_attempt != Some(attempt.into_uuid())
        || stored_turn != Some(turn.into_uuid())
        || stored_session != Some(session.into_uuid())
    {
        return Err(SubmitInputCorruption::Inconsistent("active current attempt").into());
    }
    Ok(())
}

pub(super) fn map_tool_loop_error(
    error: crate::tool_loop::ToolLoopRepositoryError,
) -> SubmitInputRepositoryError {
    match error {
        crate::tool_loop::ToolLoopRepositoryError::Database { source, .. } => source.into(),
        crate::tool_loop::ToolLoopRepositoryError::IdentityCollision
        | crate::tool_loop::ToolLoopRepositoryError::Corruption(_)
        | crate::tool_loop::ToolLoopRepositoryError::DifferentCommandKind
        | crate::tool_loop::ToolLoopRepositoryError::ConflictingCommandReuse
        | crate::tool_loop::ToolLoopRepositoryError::InvalidTransition(_) => {
            SubmitInputCorruption::Inconsistent("active tool batch").into()
        }
    }
}

pub(crate) fn require_applied_interrupt_from_attempt(
    row: &PgRow,
    owning_turn: TurnId,
    recorded_commands: &BTreeMap<DurableCommandId, ReconstitutedSubmitInput>,
) -> Result<AppliedInterruptCommandResult, SubmitInputRepositoryError> {
    let command = durable_command_id_from_uuid(required(row, "interrupt_command_id")?)
        .map_err(|_| SubmitInputCorruption::Inconsistent("interrupt command identity"))?;
    let predecessor = turn_id_from_uuid(required(row, "attempt_interrupt_predecessor_turn_id")?);
    if predecessor != owning_turn {
        return Err(SubmitInputCorruption::Inconsistent("attempt interrupt predecessor").into());
    }
    let receipt = recorded_commands
        .get(&command)
        .ok_or(SubmitInputCorruption::Missing("applied interrupt command"))?;
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        return Err(
            SubmitInputCorruption::Inconsistent("interrupt command was not applied").into(),
        );
    };
    origin
        .applied_interrupt()
        .copied()
        .filter(|interrupt| {
            interrupt.proof().command() == command && interrupt.proof().predecessor() == owning_turn
        })
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("attempt interrupt authority").into())
}

pub(super) fn applied_interrupt_for_turn(
    owning_turn: TurnId,
    recorded_commands: &BTreeMap<DurableCommandId, ReconstitutedSubmitInput>,
) -> Result<Option<AppliedInterruptCommandResult>, SubmitInputRepositoryError> {
    let mut matches = recorded_commands.values().filter_map(|receipt| {
        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) =
            receipt.result()
        else {
            return None;
        };
        origin
            .applied_interrupt()
            .copied()
            .filter(|interrupt| interrupt.proof().predecessor() == owning_turn)
    });
    let interrupt = matches.next();
    if matches.next().is_some() {
        return Err(
            SubmitInputCorruption::Inconsistent("multiple applied interrupt commands").into(),
        );
    }
    Ok(interrupt)
}

pub(super) fn require_applied_runner_recovery_interrupt(
    row: &PgRow,
    owning_turn: TurnId,
    yielded_attempt: TurnAttemptId,
    interrupted_tool_attempt: Option<Uuid>,
    recorded_commands: &BTreeMap<DurableCommandId, ReconstitutedSubmitInput>,
) -> Result<AppliedInterruptCommandResult, SubmitInputRepositoryError> {
    let command =
        durable_command_id_from_uuid(required(row, "runner_recovery_interrupt_command_id")?)
            .map_err(|_| SubmitInputCorruption::Inconsistent("runner recovery command identity"))?;
    let recorded_yielded_attempt: Uuid = required(row, "runner_recovery_yielded_attempt_id")?;
    let recorded_interrupted_attempt: Option<Uuid> =
        row.try_get("runner_recovery_interrupted_tool_attempt_id")?;
    if recorded_yielded_attempt != yielded_attempt.into_uuid()
        || recorded_interrupted_attempt != interrupted_tool_attempt
    {
        return Err(SubmitInputCorruption::Inconsistent("runner recovery interrupt effect").into());
    }
    let receipt = recorded_commands
        .get(&command)
        .ok_or(SubmitInputCorruption::Missing(
            "runner recovery interrupt command",
        ))?;
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        return Err(SubmitInputCorruption::Inconsistent(
            "runner recovery interrupt was not applied",
        )
        .into());
    };
    origin
        .applied_interrupt()
        .copied()
        .filter(|interrupt| {
            interrupt.proof().command() == command && interrupt.proof().predecessor() == owning_turn
        })
        .ok_or_else(|| {
            SubmitInputCorruption::Inconsistent("runner recovery interrupt authority").into()
        })
}

pub(super) fn accepted_origin_source_turn(delivery: DeliveryRequest) -> Option<TurnId> {
    match delivery {
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            ..
        }
        | DeliveryRequest::Interrupt {
            expected_active_turn,
            ..
        } => Some(expected_active_turn),
        DeliveryRequest::StartWhenNoActiveTurn { .. } | DeliveryRequest::NextSafePoint { .. } => {
            None
        }
    }
}

pub(crate) fn decode_goal_origin_configuration(
    row: &PgRow,
    expected_session: SessionId,
) -> Result<OriginConfiguration, SubmitInputRepositoryError> {
    let defaults_session = session_id_from_uuid(required(row, "goal_defaults_session_id")?);
    if defaults_session != expected_session {
        return Err(SubmitInputCorruption::Inconsistent("goal defaults session").into());
    }
    let queued_version = decode_defaults_version(row, "queued_defaults_version")?;
    let defaults_version = decode_defaults_version(row, "goal_defaults_version")?;
    if defaults_version != queued_version {
        return Err(SubmitInputCorruption::Inconsistent("goal defaults version").into());
    }
    let defaults = decode_defaults(
        required(row, "goal_defaults_model_kind")?,
        row.try_get("goal_defaults_direct_id")?,
        row.try_get("goal_defaults_alias_id")?,
        required(row, "goal_defaults_tool_auto_approval")?,
        required(row, "goal_defaults_model_settings")?,
        "goal defaults",
    )?;
    let requested = decode_model_selection(
        required(row, "requested_model_kind")?,
        row.try_get("requested_direct_model_selection_id")?,
        row.try_get("requested_model_alias_id")?,
        "goal requested model",
    )?;
    let frozen = decode_frozen_model(
        required(row, "frozen_model_kind")?,
        row.try_get("frozen_direct_model_selection_id")?,
        row.try_get("frozen_model_alias_id")?,
        row.try_get("frozen_alias_selected_direct_id")?,
    )?;
    OriginConfigurationReconstitutionInput::new(defaults_version, defaults, requested, frozen)
        .reconstitute()
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("goal origin configuration").into())
}

pub(super) fn require_stored_origin_configuration(
    row: &PgRow,
    expected: &OriginConfiguration,
) -> Result<(), SubmitInputRepositoryError> {
    let source: Option<Uuid> = row.try_get("source_configuration_turn_id")?;
    if source.is_some() {
        return Err(
            SubmitInputCorruption::Inconsistent("explicit configuration source reference").into(),
        );
    }
    require_spelling(row, "model_parameters", "provider_defaults")?;
    require_spelling(row, "known_provider_failure_retry", "disabled")?;
    require_spelling(row, "model_fallback", "disabled")?;
    let dangerous_tool_auto_approval = decode_dangerous_tool_auto_approval(
        row,
        "queued_tool_auto_approval",
        "scheduling origin configuration",
    )?;
    let defaults_version = decode_defaults_version(row, "queued_defaults_version")?;
    let requested = decode_model_selection(
        required(row, "requested_model_kind")?,
        row.try_get("requested_direct_model_selection_id")?,
        row.try_get("requested_model_alias_id")?,
        "scheduling requested model",
    )?;
    let frozen = decode_frozen_model(
        required(row, "frozen_model_kind")?,
        row.try_get("frozen_direct_model_selection_id")?,
        row.try_get("frozen_model_alias_id")?,
        row.try_get("frozen_alias_selected_direct_id")?,
    )?;
    if defaults_version != expected.session_defaults_version()
        || requested != expected.requested().model()
        || frozen != *expected.effective().model()
        || dangerous_tool_auto_approval != expected.effective().dangerous_tool_auto_approval()
    {
        return Err(SubmitInputCorruption::Inconsistent("scheduling origin configuration").into());
    }
    Ok(())
}

pub(super) fn require_stored_inherited_configuration(
    row: &PgRow,
    expected_source: TurnId,
) -> Result<(), SubmitInputRepositoryError> {
    let source: Option<Uuid> = row.try_get("source_configuration_turn_id")?;
    let values_absent: bool = required(row, "queued_configuration_values_absent")?;
    if source != Some(expected_source.into_uuid()) || !values_absent {
        return Err(
            SubmitInputCorruption::Inconsistent("inherited configuration provenance").into(),
        );
    }
    Ok(())
}

pub(super) enum StoredOriginRuntimeState {
    RuntimeRelevant,
    RetiredGoal,
}

pub(super) fn decode_origin_runtime_state(
    value: &str,
) -> Result<StoredOriginRuntimeState, SubmitInputRepositoryError> {
    match value {
        "runtime_relevant" => Ok(StoredOriginRuntimeState::RuntimeRelevant),
        "retired_goal" => Ok(StoredOriginRuntimeState::RetiredGoal),
        value => Err(SubmitInputCorruption::Unsupported {
            field: "acceptance-tail origin runtime state",
            value: value.to_owned(),
        }
        .into()),
    }
}

pub(super) fn decode_complete(
    row: PgRow,
    command_id: DurableCommandId,
    related_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    non_accepted_predecessor: Option<NonAcceptedTurnPredecessorReconstitutionInput>,
    existing_interrupt: Option<AppliedInterruptCommandResult>,
) -> Result<ReconstitutedSubmitInput, SubmitInputRepositoryError> {
    require_spelling(&row, "registry_kind", SUBMIT_INPUT_KIND)?;
    let registry_version = require_supported_version(&row, "registry_version")?;
    let typed_id: Uuid = required(&row, "typed_command_id")?;
    if typed_id != durable_command_id_to_uuid(command_id) {
        return Err(SubmitInputCorruption::Inconsistent("typed command identity").into());
    }
    require_spelling(&row, "typed_kind", SUBMIT_INPUT_KIND)?;
    let typed_version = require_supported_version(&row, "typed_version")?;
    if registry_version != typed_version {
        return Err(SubmitInputCorruption::Inconsistent("command storage version").into());
    }

    // Decode-level checks reject unknown or malformed actor spellings here;
    // comparing the decoded actor against the canonical command's actor is
    // domain-owned semantics and happens inside reconstitution.
    let actor = decode_actor(
        required(&row, "actor_kind")?,
        row.try_get("actor_turn_id")?,
        row.try_get("actor_tool_request_id")?,
        row.try_get("actor_program_run_id")?,
        row.try_get("verified_actor_program_run_id")?,
        typed_version,
    )?;
    let command_model_settings_override: Value = required(&row, "command_model_settings_override")?;
    let session = session_id_from_uuid(required(&row, "command_session_id")?);
    let content = decode_content(required(&row, "command_content_parts")?, "command content")?;
    let delivery = decode_delivery(
        required(&row, "command_delivery_kind")?,
        row.try_get("command_descendant_scope")?,
        row.try_get("command_expected_active_turn_id")?,
        row.try_get("command_expected_defaults_version")?,
        row.try_get("command_model_override_kind")?,
        row.try_get("command_replacement_model_kind")?,
        row.try_get("command_replacement_direct_id")?,
        row.try_get("command_replacement_alias_id")?,
        command_model_settings_override,
        "command delivery",
    )?;
    let command = match actor {
        Actor::User => SubmitInput::new(command_id, session, content, delivery),
        Actor::Program { run } => {
            SubmitInput::new_program(command_id, session, content, delivery, run)
        }
        Actor::Model { turn } => {
            SubmitInput::from_recorded_model(command_id, session, content, delivery, turn)
        }
        Actor::Tool { request } => {
            SubmitInput::from_recorded_tool(command_id, session, content, delivery, request)
        }
        Actor::Recovery => {
            SubmitInput::from_recorded_recovery(command_id, session, content, delivery)
        }
        Actor::Core => match delivery {
            DeliveryRequest::StartWhenNoActiveTurn { configuration } => {
                SubmitInput::new_core_continuation(command_id, session, content, configuration)
            }
            DeliveryRequest::Interrupt {
                expected_active_turn,
                descendant_scope,
                configuration,
            } => SubmitInput::new_core_interrupt(
                command_id,
                session,
                content,
                expected_active_turn,
                descendant_scope,
                configuration,
            ),
            DeliveryRequest::NextSafePoint { .. } | DeliveryRequest::AfterCurrentTurn { .. } => {
                return Err(SubmitInputCorruption::Inconsistent("core input delivery").into());
            }
        },
    };

    let result_kind: String = required(&row, "result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let result_session = session_id_from_uuid(required(&row, "result_session_id")?);
    let result_accepted: Option<Uuid> = row.try_get("result_accepted_input_id")?;
    let result_turn: Option<Uuid> = row.try_get("result_turn_id")?;
    let result_actual_turn: Option<Uuid> = row.try_get("result_actual_active_turn_id")?;
    let result_expected_turn: Option<Uuid> = row.try_get("result_expected_active_turn_id")?;
    let result_expected_defaults: Option<Decimal> =
        row.try_get("result_expected_defaults_version")?;
    let result_current_defaults: Option<Decimal> =
        row.try_get("result_current_defaults_version")?;
    let result_unknown_alias: Option<Uuid> = row.try_get("result_unknown_alias_id")?;
    let result_selected_defaults: Option<Decimal> =
        row.try_get("result_selected_defaults_version")?;
    let result_last_position: Option<Decimal> = row.try_get("result_last_position")?;
    let result_existing_interrupt: Option<Uuid> =
        row.try_get("result_existing_interrupt_command_id")?;
    let result_attachment_digest: Option<Vec<u8>> = row.try_get("result_attachment_digest")?;
    let result_attachment_maximum_bytes: Option<Decimal> =
        row.try_get("result_attachment_maximum_bytes")?;
    let accepted_effect_count: i64 = required(&row, "accepted_effect_count")?;
    let queued_effect_count: i64 = required(&row, "queued_effect_count")?;

    let input = match (result_kind.as_str(), rejection_kind.as_deref()) {
        (APPLIED, None) => {
            if result_expected_turn.is_some()
                || result_expected_defaults.is_some()
                || result_current_defaults.is_some()
                || result_unknown_alias.is_some()
                || result_selected_defaults.is_some()
                || result_last_position.is_some()
                || result_existing_interrupt.is_some()
                || result_attachment_digest.is_some()
                || result_attachment_maximum_bytes.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("applied result fields").into());
            }
            if accepted_effect_count != 1 {
                return Err(
                    SubmitInputCorruption::Inconsistent("applied effect cardinality").into(),
                );
            }
            let result_accepted = accepted_input_id_from_uuid(
                result_accepted
                    .ok_or(SubmitInputCorruption::Missing("result_accepted_input_id"))?,
            );
            match (result_turn, result_actual_turn) {
                (Some(result_turn), None) if queued_effect_count == 1 => {
                    decode_applied_turn_origin(
                        &row,
                        command,
                        actor,
                        result_session,
                        result_accepted,
                        turn_id_from_uuid(result_turn),
                        RelatedTurnEvidence {
                            origin: related_turn_origin,
                            non_accepted_predecessor,
                        },
                    )?
                }
                (None, Some(source_turn)) if queued_effect_count <= 1 => {
                    let source_turn_origin = related_turn_origin.ok_or(
                        SubmitInputCorruption::Missing("pending steering source turn origin"),
                    )?;
                    if non_accepted_predecessor.is_some() {
                        return Err(SubmitInputCorruption::Inconsistent(
                            "pending steering non-accepted predecessor",
                        )
                        .into());
                    }
                    decode_applied_pending_steering(
                        &row,
                        command,
                        actor,
                        result_session,
                        result_accepted,
                        turn_id_from_uuid(source_turn),
                        source_turn_origin,
                    )?
                }
                _ => {
                    return Err(
                        SubmitInputCorruption::Inconsistent("applied variant correlation").into(),
                    );
                }
            }
        }
        (REJECTED, Some(kind)) => {
            if non_accepted_predecessor.is_some() {
                return Err(SubmitInputCorruption::Inconsistent(
                    "rejected command non-accepted predecessor",
                )
                .into());
            }
            if accepted_effect_count != 0 || queued_effect_count != 0 {
                return Err(
                    SubmitInputCorruption::Inconsistent("rejected command has effects").into(),
                );
            }
            if result_accepted.is_some() || result_turn.is_some() {
                return Err(
                    SubmitInputCorruption::Inconsistent("rejected applied identities").into(),
                );
            }
            decode_rejected(
                &row,
                command,
                actor,
                result_session,
                related_turn_origin,
                kind,
                result_actual_turn,
                result_expected_turn,
                result_expected_defaults,
                result_current_defaults,
                result_unknown_alias,
                result_selected_defaults,
                result_last_position,
                result_existing_interrupt,
                result_attachment_digest,
                result_attachment_maximum_bytes,
                existing_interrupt,
            )?
        }
        (APPLIED, Some(_)) | (REJECTED, None) => {
            return Err(SubmitInputCorruption::Inconsistent("terminal result shape").into());
        }
        (value, _) => {
            return Err(SubmitInputCorruption::Unsupported {
                field: "result_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };

    input
        .reconstitute()
        .map_err(|error| SubmitInputCorruption::Domain(error.failure()).into())
}

fn decode_applied_turn_origin(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_turn: TurnId,
    predecessor: RelatedTurnEvidence,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    let accepting_command_uuid: Uuid = required(row, "accepting_command_id")?;
    let accepting_command = durable_command_id_from_uuid(accepting_command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("accepting command identity"))?;
    let accepted_input = accepted_input_id_from_uuid(required(row, "accepted_input_id")?);
    let accepted_session = session_id_from_uuid(required(row, "accepted_session_id")?);
    let accepted_content =
        decode_content(required(row, "accepted_content_parts")?, "accepted content")?;
    let accepted_delivery = decode_delivery(
        required(row, "accepted_delivery_kind")?,
        row.try_get("accepted_descendant_scope")?,
        row.try_get("accepted_expected_active_turn_id")?,
        row.try_get("accepted_expected_defaults_version")?,
        row.try_get("accepted_model_override_kind")?,
        row.try_get("accepted_replacement_model_kind")?,
        row.try_get("accepted_replacement_direct_id")?,
        row.try_get("accepted_replacement_alias_id")?,
        required(row, "accepted_model_settings_override")?,
        "accepted delivery",
    )?;
    let accepted_position = decode_position(row, "accepted_position")?;
    require_spelling(row, "disposition_kind", "origin_of")?;
    let accepted_origin_turn = turn_id_from_uuid(required(row, "origin_turn_id")?);

    let queued_turn = turn_id_from_uuid(required(row, "queued_turn_id")?);
    let queued_accepted = accepted_input_id_from_uuid(required(row, "queued_accepted_input_id")?);
    if queued_accepted != accepted_input {
        return Err(SubmitInputCorruption::Inconsistent("queued accepted input").into());
    }
    let queued_session = session_id_from_uuid(required(row, "queued_session_id")?);
    let queued_position = decode_position(row, "queued_position")?;
    let queue_order = match required::<String>(row, "priority_kind")?.as_str() {
        "ordinary" => {
            if row
                .try_get::<Option<Uuid>, _>("interrupt_predecessor_turn_id")?
                .is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("ordinary queue priority").into());
            }
            AcceptedInputQueueOrder::ordinary(queued_position)
        }
        "interrupt_immediately_after" => AcceptedInputQueueOrder::interrupt_immediately_after(
            queued_position,
            turn_id_from_uuid(required(row, "interrupt_predecessor_turn_id")?),
        ),
        value => {
            return Err(SubmitInputCorruption::Unsupported {
                field: "priority_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    require_spelling(row, "model_parameters", "provider_defaults")?;
    require_spelling(row, "known_provider_failure_retry", "disabled")?;
    require_spelling(row, "model_fallback", "disabled")?;
    let defaults_version = decode_defaults_version(row, "queued_defaults_version")?;
    let model_settings_evidence_required: bool =
        required(row, "origin_model_settings_evidence_required")?;

    let defaults_session = session_id_from_uuid(required(row, "defaults_session_id")?);
    let joined_defaults_version = decode_defaults_version(row, "defaults_version")?;
    if joined_defaults_version != defaults_version {
        return Err(SubmitInputCorruption::Inconsistent("selected defaults version").into());
    }
    let defaults = decode_defaults(
        required(row, "defaults_model_kind")?,
        row.try_get("defaults_direct_id")?,
        row.try_get("defaults_alias_id")?,
        required(row, "defaults_tool_auto_approval")?,
        required(row, "defaults_model_settings")?,
        "selected defaults",
    )?;
    let stored_requested_model = decode_model_selection(
        required(row, "requested_model_kind")?,
        row.try_get("requested_direct_model_selection_id")?,
        row.try_get("requested_model_alias_id")?,
        "requested model",
    )?;
    let stored_frozen_model = decode_frozen_model(
        required(row, "frozen_model_kind")?,
        row.try_get("frozen_direct_model_selection_id")?,
        row.try_get("frozen_model_alias_id")?,
        row.try_get("frozen_alias_selected_direct_id")?,
    )?;
    let stored_settings_selection: Option<Uuid> = row.try_get("settings_selected_direct_id")?;
    let stored_settings_session: Option<Uuid> = row.try_get("settings_session_id")?;
    let stored_settings_defaults: Option<Decimal> = row.try_get("settings_defaults_version")?;
    let stored_per_call_settings: Option<Value> = row.try_get("per_call_model_settings")?;
    let stored_resolved_settings: Option<Value> = row.try_get("resolved_model_settings")?;
    let stored_adjusted_from_selection: Option<Uuid> = row.try_get("adjusted_from_selection_id")?;
    let stored_adjustments: Option<Value> = row.try_get("model_settings_adjustments")?;
    let (stored_model_settings, stored_model_settings_adjustments) = match (
        stored_settings_selection,
        stored_settings_session,
        stored_settings_defaults,
        stored_per_call_settings,
        stored_resolved_settings,
        stored_adjustments,
    ) {
        (None, None, None, None, None, None) => (None, Vec::new()),
        (
            Some(selection),
            Some(settings_session),
            Some(settings_defaults),
            Some(per_call),
            Some(settings),
            Some(adjustments),
        ) => {
            let per_call = model_settings_overlay_from_json(per_call)
                .map_err(|_| SubmitInputCorruption::Inconsistent("per-call model settings"))?;
            let settings = model_settings_from_json(settings)
                .map_err(|_| SubmitInputCorruption::Inconsistent("resolved model settings"))?;
            let adjustments = model_change_adjustments_from_json(adjustments)
                .map_err(|_| SubmitInputCorruption::Inconsistent("model settings adjustments"))?;
            let adjusted_from_selection =
                stored_adjusted_from_selection.map(DirectModelSelection::from_uuid);
            let expected_adjusted_from_selection = (!adjustments.is_empty())
                .then_some(defaults.model_settings().validated_for())
                .flatten();
            if DirectModelSelection::from_uuid(selection) != stored_frozen_model.selected_direct()
                || session_id_from_uuid(settings_session) != result_session
                || defaults_version_from_numeric(settings_defaults).map_err(|reason| {
                    SubmitInputCorruption::InvalidOrdinal {
                        field: "settings defaults version",
                        reason,
                    }
                })? != defaults_version
                || per_call
                    != configured_model_settings(accepted_delivery).ok_or(
                        SubmitInputCorruption::Inconsistent("origin delivery settings"),
                    )?
                || adjusted_from_selection != expected_adjusted_from_selection
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "resolved model settings correlation",
                )
                .into());
            }
            (Some(settings), adjustments)
        }
        _ => {
            return Err(
                SubmitInputCorruption::Inconsistent("resolved model settings event shape").into(),
            );
        }
    };
    if model_settings_evidence_required && stored_model_settings.is_none() {
        return Err(SubmitInputCorruption::Missing("turn model settings evidence").into());
    }
    Ok(SubmitInputReconstitutionInput::applied_turn_origin(
        SubmitInputAppliedTurnOriginReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_accepted_input,
            result_turn,
            predecessor_origin: predecessor.origin,
            non_accepted_predecessor: predecessor.non_accepted_predecessor,
            accepted_command: accepting_command,
            accepted_input,
            accepted_session,
            accepted_content,
            accepted_delivery,
            accepted_position,
            accepted_disposition: AcceptedInputDisposition::OriginOf(accepted_origin_turn),
            queue_session: queued_session,
            queue_turn: queued_turn,
            queue_order,
            defaults_session,
            defaults_version,
            defaults,
            stored_requested_model,
            stored_frozen_model,
            stored_model_settings,
            stored_model_settings_adjustments,
        },
    ))
}

fn decode_applied_pending_steering(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_source_turn: TurnId,
    source_turn_origin: SubmitInputTurnOriginReconstitutionInput,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    let accepting_command_uuid: Uuid = required(row, "accepting_command_id")?;
    let accepting_command = durable_command_id_from_uuid(accepting_command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("accepting command identity"))?;
    let accepted_input = accepted_input_id_from_uuid(required(row, "accepted_input_id")?);
    let accepted_session = session_id_from_uuid(required(row, "accepted_session_id")?);
    let accepted_content =
        decode_content(required(row, "accepted_content_parts")?, "accepted content")?;
    let accepted_delivery = decode_delivery(
        required(row, "accepted_delivery_kind")?,
        row.try_get("accepted_descendant_scope")?,
        row.try_get("accepted_expected_active_turn_id")?,
        row.try_get("accepted_expected_defaults_version")?,
        row.try_get("accepted_model_override_kind")?,
        row.try_get("accepted_replacement_model_kind")?,
        row.try_get("accepted_replacement_direct_id")?,
        row.try_get("accepted_replacement_alias_id")?,
        required(row, "accepted_model_settings_override")?,
        "accepted delivery",
    )?;
    let accepted_position = decode_position(row, "accepted_position")?;

    Ok(SubmitInputReconstitutionInput::applied_pending_steering(
        SubmitInputAppliedPendingSteeringReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_accepted_input,
            result_source_turn,
            source_turn_origin,
            accepted_command: accepting_command,
            accepted_input,
            accepted_session,
            accepted_content,
            accepted_delivery,
            accepted_position,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn decode_rejected(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    rejection_kind: &str,
    actual_turn: Option<Uuid>,
    expected_turn: Option<Uuid>,
    expected_defaults: Option<Decimal>,
    current_defaults: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults: Option<Decimal>,
    last_position: Option<Decimal>,
    existing_interrupt_command: Option<Uuid>,
    attachment_digest: Option<Vec<u8>>,
    attachment_maximum_bytes: Option<Decimal>,
    existing_interrupt: Option<AppliedInterruptCommandResult>,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    if !matches!(
        rejection_kind,
        "safe_point_unavailable_while_stopping" | "interrupt_already_applied"
    ) && (existing_interrupt_command.is_some() || existing_interrupt.is_some())
    {
        return Err(
            SubmitInputCorruption::Inconsistent("unexpected existing interrupt result").into(),
        );
    }
    if !matches!(
        rejection_kind,
        "attachment_blob_not_found" | "attachment_byte_budget_exceeded"
    ) && (attachment_digest.is_some() || attachment_maximum_bytes.is_some())
    {
        return Err(
            SubmitInputCorruption::Inconsistent("unexpected attachment result evidence").into(),
        );
    }
    match rejection_kind {
        "attachment_blob_not_found" => {
            require_all_absent(
                actual_turn,
                expected_turn,
                expected_defaults,
                current_defaults,
                unknown_alias,
                selected_defaults,
                last_position,
                "attachment-blob-not-found result fields",
            )?;
            if attachment_maximum_bytes.is_some() {
                return Err(SubmitInputCorruption::Inconsistent(
                    "attachment-blob-not-found maximum bytes",
                )
                .into());
            }
            let digest = attachment_digest
                .ok_or(SubmitInputCorruption::Missing("result_attachment_digest"))?;
            let digest = <[u8; 32]>::try_from(digest)
                .map_err(|_| SubmitInputCorruption::Inconsistent("result attachment digest"))?;
            Ok(
                SubmitInputReconstitutionInput::rejected_attachment_blob_not_found(
                    SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_digest: signalbox_domain::BlobDigest::from_bytes(digest),
                    },
                ),
            )
        }
        "attachment_byte_budget_exceeded" => {
            require_all_absent(
                actual_turn,
                expected_turn,
                expected_defaults,
                current_defaults,
                unknown_alias,
                selected_defaults,
                last_position,
                "attachment-byte-budget-exceeded result fields",
            )?;
            if attachment_digest.is_some() {
                return Err(SubmitInputCorruption::Inconsistent(
                    "attachment-byte-budget-exceeded digest",
                )
                .into());
            }
            let maximum_bytes = positive_u64_from_numeric(attachment_maximum_bytes.ok_or(
                SubmitInputCorruption::Missing("result_attachment_maximum_bytes"),
            )?)
            .map_err(|_| SubmitInputCorruption::Inconsistent("result attachment maximum bytes"))?;
            Ok(
                SubmitInputReconstitutionInput::rejected_attachment_byte_budget_exceeded(
                    SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_maximum_bytes: maximum_bytes,
                    },
                ),
            )
        }
        "session_not_found" => {
            require_all_absent(
                actual_turn,
                expected_turn,
                expected_defaults,
                current_defaults,
                unknown_alias,
                selected_defaults,
                last_position,
                "session-not-found result fields",
            )?;
            Ok(SubmitInputReconstitutionInput::rejected_session_not_found(
                SubmitInputRejectedSessionNotFoundReconstitutionInput {
                    command,
                    stored_actor,
                    result_session,
                },
            ))
        }
        "no_active_turn" => {
            if actual_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("no-active-turn result fields").into(),
                );
            }
            Ok(SubmitInputReconstitutionInput::rejected_no_active_turn(
                SubmitInputRejectedNoActiveTurnReconstitutionInput {
                    command,
                    stored_actor,
                    result_session,
                    result_expected_active_turn: turn_id_from_uuid(expected_turn.ok_or(
                        SubmitInputCorruption::Missing("result_expected_active_turn_id"),
                    )?),
                },
            ))
        }
        "active_turn_present" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active-turn-present result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_active_turn_present(
                    SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_active_turn: turn_id_from_uuid(actual_turn.ok_or(
                            SubmitInputCorruption::Missing("result_actual_active_turn_id"),
                        )?),
                        active_turn_origin: active_turn_origin
                            .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                    },
                ),
            )
        }
        "active_turn_mismatch" => {
            if expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active-turn-mismatch result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                    SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_expected_active_turn: turn_id_from_uuid(expected_turn.ok_or(
                            SubmitInputCorruption::Missing("result_expected_active_turn_id"),
                        )?),
                        result_actual_active_turn: turn_id_from_uuid(actual_turn.ok_or(
                            SubmitInputCorruption::Missing("result_actual_active_turn_id"),
                        )?),
                        actual_turn_origin: active_turn_origin
                            .ok_or(SubmitInputCorruption::Missing("actual turn origin"))?,
                    },
                ),
            )
        }
        "session_defaults_version_mismatch" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("defaults-mismatch result fields").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                    SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_expected: decode_optional_defaults_version(
                            expected_defaults,
                            "result_expected_defaults_version",
                        )?
                        .ok_or(SubmitInputCorruption::Missing(
                            "result_expected_defaults_version",
                        ))?,
                        result_current: decode_optional_defaults_version(
                            current_defaults,
                            "result_current_defaults_version",
                        )?
                        .ok_or(SubmitInputCorruption::Missing(
                            "result_current_defaults_version",
                        ))?,
                        active_turn_origin,
                    },
                ),
            )
        }
        "unknown_model_alias" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("unknown-alias result fields").into(),
                );
            }
            let selected = decode_optional_defaults_version(
                selected_defaults,
                "result_selected_defaults_version",
            )?
            .ok_or(SubmitInputCorruption::Missing(
                "result_selected_defaults_version",
            ))?;
            let defaults_session = session_id_from_uuid(required(row, "defaults_session_id")?);
            let defaults_version = decode_defaults_version(row, "defaults_version")?;
            let defaults = decode_defaults(
                required(row, "defaults_model_kind")?,
                row.try_get("defaults_direct_id")?,
                row.try_get("defaults_alias_id")?,
                required(row, "defaults_tool_auto_approval")?,
                required(row, "defaults_model_settings")?,
                "selected defaults",
            )?;
            if selected != defaults_version {
                return Err(
                    SubmitInputCorruption::Inconsistent("unknown-alias defaults version").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                    SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_alias: ModelAlias::from_uuid(
                            unknown_alias
                                .ok_or(SubmitInputCorruption::Missing("result_unknown_alias_id"))?,
                        ),
                        defaults_session,
                        defaults_version,
                        defaults,
                        active_turn_origin,
                    },
                ),
            )
        }
        "acceptance_position_exhausted" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "position-exhausted result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                    SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_last_position: decode_optional_position(
                            last_position,
                            "result_last_position",
                        )?
                        .ok_or(SubmitInputCorruption::Missing("result_last_position"))?,
                        active_turn_origin,
                    },
                ),
            )
        }
        "safe_point_unavailable_while_stopping" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "stopping safe-point result fields",
                )
                .into());
            }
            let active_turn = turn_id_from_uuid(actual_turn.ok_or(
                SubmitInputCorruption::Missing("result_actual_active_turn_id"),
            )?);
            let stored_command = durable_command_id_from_uuid(existing_interrupt_command.ok_or(
                SubmitInputCorruption::Missing("result_existing_interrupt_command_id"),
            )?)
            .map_err(|_| {
                SubmitInputCorruption::Inconsistent("existing interrupt command identity")
            })?;
            let interrupt = existing_interrupt.ok_or(SubmitInputCorruption::Missing(
                "existing interrupt authority",
            ))?;
            if stored_command != interrupt.proof().command() {
                return Err(
                    SubmitInputCorruption::Inconsistent("existing interrupt command").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_safe_point_unavailable_while_stopping(
                    SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_active_turn: active_turn,
                        active_turn_origin: active_turn_origin
                            .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                        existing_interrupt: interrupt,
                    },
                ),
            )
        }
        "interrupt_already_applied" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "already-applied interrupt result fields",
                )
                .into());
            }
            let active_turn = turn_id_from_uuid(actual_turn.ok_or(
                SubmitInputCorruption::Missing("result_actual_active_turn_id"),
            )?);
            let stored_command = durable_command_id_from_uuid(existing_interrupt_command.ok_or(
                SubmitInputCorruption::Missing("result_existing_interrupt_command_id"),
            )?)
            .map_err(|_| {
                SubmitInputCorruption::Inconsistent("existing interrupt command identity")
            })?;
            let interrupt = existing_interrupt.ok_or(SubmitInputCorruption::Missing(
                "existing interrupt authority",
            ))?;
            Ok(
                SubmitInputReconstitutionInput::rejected_interrupt_already_applied(
                    SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput {
                        command,
                        stored_actor,
                        result_session,
                        result_active_turn: active_turn,
                        result_existing_command: stored_command,
                        active_turn_origin: active_turn_origin
                            .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                        existing_interrupt: interrupt,
                    },
                ),
            )
        }
        "interrupt_unavailable_while_awaiting_approval" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "parked-approval interrupt result fields",
                )
                .into());
            }
            let active_turn = turn_id_from_uuid(actual_turn.ok_or(
                SubmitInputCorruption::Missing("result_actual_active_turn_id"),
            )?);
            let input =
                SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
                    command,
                    stored_actor,
                    result_session,
                    result_active_turn: active_turn,
                    active_turn_origin: active_turn_origin
                        .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                };
            Ok(
                SubmitInputReconstitutionInput::rejected_interrupt_unavailable_while_awaiting_approval(
                    input,
                ),
            )
        }
        value => Err(SubmitInputCorruption::Unsupported {
            field: "rejection_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}
