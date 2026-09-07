use super::decode::{
    accepted_origin_source_turn, applied_interrupt_for_turn,
    decode_delegated_turn_scheduling_state, decode_delegation_outcome_kind,
    decode_delegation_outcome_reason, decode_delegation_provenance, decode_delegation_wait_mode,
    decode_goal_origin_configuration, delegation_content, map_imported_scheduling_error,
    map_tool_loop_error, require_applied_interrupt_from_attempt,
    require_applied_runner_recovery_interrupt, require_current_attempt_row,
    require_stored_inherited_configuration, require_stored_origin_configuration,
};
use super::write::{decode_starting_lineage, load_active_acceptance_tail};
use super::{
    StoredSchedulingInventoryCounts, SubmitInputCorruption, SubmitInputRepositoryError,
    decode_frozen_model, decode_model_call_disposition, decode_optional_token_count,
    decode_position, require_recorded_batch, required,
};
use crate::mapping::{
    accepted_input_id_from_uuid, defaults_version_from_numeric, durable_command_id_from_uuid,
    positive_u64_from_numeric, session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid,
};
use crate::tool_loop::{
    load_active_batch_from_connection, load_continuation_round_evidence,
    load_recovery_batch_by_attempt, load_runner_recovery_source_snapshot,
    load_steering_continuation_round_evidence, load_terminal_result_attempts,
    load_terminal_result_denials,
};
use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionInput,
    AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState,
    ActiveTurnSchedulingReconstitutionInput, AssistantText, AutomaticReconciliationAuthority,
    CancellationStopDisposition, CancelledTurnExecutionReconstitutionInput,
    ConsumedSteeringReconstitutionInput, ContextCompactionId,
    ContextCompactionModelCallReconstitutionInput, ContextCompactionModelCallState,
    ContextCompactionRange, ContextCompactionReconstitutionInput, ContextCompactionTokenUsage,
    ContextFrontierId, ContinuationRoundReconstitutionInput, DelegatedTurnSchedulingFact,
    DelegationMessageId, DelegationOutcome, DelegationWaitMode, DeliveryRequest,
    DirectModelSelection, FailedTurnExecutionReconstitutionInput, ModelCallId,
    ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelSelectionOverride,
    OriginConfiguration, PerInputConfigurationChoices, PinnedProviderTargetReconstitutionInput,
    ProviderCompactionBlock, ProviderModelIdentity, ProviderReasoningItem,
    ResolvedContextFrontierReconstitutionInput, ResolvedProviderTarget, RunnerGeneration, RunnerId,
    SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload as InitialSemanticTranscriptEntryPayload,
    SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef, Session, SessionId,
    SteeringContinuationRoundReconstitutionInput, SteeringReclassificationReason,
    SubmitInputAppliedResult, SubmitInputResult, TerminalAttemptEndReconstitutionInput,
    ToolAttemptId, ToolRequestId, TranscriptAncestry, TurnAttemptId, TurnId,
    UnstoppedAttemptDisposition,
};
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::{NonZeroU32, NonZeroU64};

pub(crate) async fn load_scheduling_projection(
    connection: &mut PgConnection,
    session: Session,
) -> Result<AcceptedInputSchedulingProjection, SubmitInputRepositoryError> {
    load_scheduling_projection_with_semantic_frontiers(connection, session, &[]).await
}

pub(super) async fn load_scheduling_projection_with_semantic_frontiers(
    connection: &mut PgConnection,
    session: Session,
    supplemental_semantic_frontiers: &[ContextFrontierId],
) -> Result<AcceptedInputSchedulingProjection, SubmitInputRepositoryError> {
    let session_id = session.id();
    let imported_session = if matches!(
        session.creation_provenance().ancestry(),
        TranscriptAncestry::ImportedConversation { .. }
    ) {
        Some(
            crate::create_session_from_imported_frontier::load_complete_current(
                connection, &session,
            )
            .await
            .map_err(map_imported_scheduling_error)?,
        )
    } else {
        None
    };
    let inventory = sqlx::query_as::<_, StoredSchedulingInventoryCounts>(
        "SELECT
            (SELECT count(*)
               FROM queued_input_origin
              WHERE session_id = $1
                AND goal_turn_is_runtime_relevant(
                    session_id, turn_id
                )) AS queue_count,
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1
                AND origin_kind = 'accepted_input'
                AND goal_turn_is_runtime_relevant(
                    session_id, turn_id
                )) AS lifecycle_count",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_one(&mut *connection)
    .await?;
    if inventory.queue_count != inventory.lifecycle_count {
        return Err(
            SubmitInputCorruption::Inconsistent("complete scheduling turn inventory").into(),
        );
    }

    let rows = sqlx::query(
        "SELECT
            queued.turn_id AS queued_turn_id,
            queued.accepted_input_id AS queued_accepted_input_id,
            queued.session_id AS queued_session_id,
            queued.acceptance_position AS queued_position,
            queued.priority_kind,
            queued.interrupt_predecessor_turn_id,
            accepted.accepting_command_id,
            goal.goal_generation,
            accepted.accepted_input_id,
            accepted.session_id AS accepted_session_id,
            accepted.disposition_kind,
            accepted.origin_turn_id,
            accepted.expected_active_turn_id AS accepted_source_turn_id,
            queued.source_configuration_turn_id,
            (
                queued.defaults_version IS NULL
                AND queued.requested_model_kind IS NULL
                AND queued.requested_direct_model_selection_id IS NULL
                AND queued.requested_model_alias_id IS NULL
                AND queued.frozen_model_kind IS NULL
                AND queued.frozen_direct_model_selection_id IS NULL
                AND queued.frozen_model_alias_id IS NULL
                AND queued.frozen_alias_selected_direct_id IS NULL
                AND queued.model_parameters IS NULL
                AND queued.known_provider_failure_retry IS NULL
                AND queued.model_fallback IS NULL
                AND queued.dangerous_tool_auto_approval IS NULL
            ) AS queued_configuration_values_absent,
            queued.defaults_version AS queued_defaults_version,
            queued.requested_model_kind,
            queued.requested_direct_model_selection_id,
            queued.requested_model_alias_id,
            queued.frozen_model_kind,
            queued.frozen_direct_model_selection_id,
            queued.frozen_model_alias_id,
            queued.frozen_alias_selected_direct_id,
            queued.model_parameters,
            queued.known_provider_failure_retry,
            queued.model_fallback,
            queued.dangerous_tool_auto_approval AS queued_tool_auto_approval,
            goal_defaults.session_id AS goal_defaults_session_id,
            goal_defaults.version AS goal_defaults_version,
            goal_defaults.model_selection_kind AS goal_defaults_model_kind,
            goal_defaults.direct_model_selection_id AS goal_defaults_direct_id,
            goal_defaults.model_alias_id AS goal_defaults_alias_id,
            goal_defaults.dangerous_tool_auto_approval AS goal_defaults_tool_auto_approval,
            goal_defaults.model_settings AS goal_defaults_model_settings,
            turn.turn_id AS lifecycle_turn_id,
            turn.session_id AS lifecycle_session_id,
            turn.state_kind AS lifecycle_state_kind,
            turn.start_lineage_kind,
            turn.immediate_predecessor_turn_id,
            turn.starting_frontier_id,
            turn.terminal_frontier_id,
            turn.active_phase_kind,
            turn.current_attempt_id,
            turn.pinned_provider_model_identity_id,
            turn.recovery_model_call_id,
            turn.active_tool_round_call_id,
            turn.approval_tool_request_id,
            turn.recovery_tool_attempt_id,
            turn.runner_recovery_runner_id,
            turn.runner_recovery_placement_revision,
            turn.runner_recovery_tool_attempt_id,
            turn.child_wait_request_id,
            turn.model_identity_boundary_required,
            turn.terminal_attempt_id,
            turn.terminal_model_call_id,
            turn.terminal_tool_attempt_id,
            turn.terminal_disposition_kind,
            automatic_reconciliation.model_call_id
                AS automatic_reconciliation_model_call_id,
            automatic_reconciliation.tool_attempt_id
                AS automatic_reconciliation_tool_attempt_id,
            automatic_reconciliation.state_kind
                AS automatic_reconciliation_state_kind,
            automatic_reconciliation.attempt_count
                AS automatic_reconciliation_attempt_count,
            (
                SELECT call.model_call_id
                  FROM model_call AS call
                 WHERE call.turn_id = turn.turn_id
                   AND call.session_id = turn.session_id
                   AND call.state_kind = 'cancellation_requested'
            ) AS stop_requested_model_call_id,
            attempt.turn_attempt_id,
            attempt.turn_id AS attempt_turn_id,
            attempt.session_id AS attempt_session_id,
            attempt.continued_from_attempt_id,
            attempt.state_kind AS attempt_state_kind,
            attempt.interrupt_command_id,
            attempt.interrupt_predecessor_turn_id AS attempt_interrupt_predecessor_turn_id,
            attempt.end_variant,
            attempt.end_disposition,
            runner_recovery_effect.command_id AS runner_recovery_interrupt_command_id,
            runner_recovery_effect.yielded_turn_attempt_id
                AS runner_recovery_yielded_attempt_id,
            runner_recovery_effect.interrupted_tool_attempt_id
                AS runner_recovery_interrupted_tool_attempt_id
         FROM queued_input_origin AS queued
         LEFT JOIN accepted_input AS accepted
           ON accepted.accepted_input_id = queued.accepted_input_id
         LEFT JOIN goal_turn AS goal
           ON goal.accepted_input_id = accepted.accepted_input_id
          AND goal.session_id = queued.session_id
          AND goal.turn_id = queued.turn_id
         LEFT JOIN session_defaults_version AS goal_defaults
           ON goal_defaults.session_id = queued.session_id
          AND goal_defaults.version = queued.defaults_version
         LEFT JOIN turn_lifecycle AS turn
           ON turn.turn_id = queued.turn_id
         LEFT JOIN turn_attempt AS attempt
           ON attempt.turn_attempt_id = COALESCE(
                turn.current_attempt_id,
                turn.terminal_attempt_id
              )
         LEFT JOIN automatic_reconciliation AS automatic_reconciliation
           ON automatic_reconciliation.turn_id = turn.turn_id
          AND automatic_reconciliation.session_id = turn.session_id
         LEFT JOIN turn_runner_recovery_interrupt_effect AS runner_recovery_effect
           ON runner_recovery_effect.turn_id = turn.turn_id
          AND runner_recovery_effect.session_id = turn.session_id
        WHERE queued.session_id = $1
          AND goal_turn_is_runtime_relevant(
                queued.session_id, queued.turn_id
          )
        ORDER BY queued.acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut accepting_commands = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Some(command_uuid) = row.try_get::<Option<Uuid>, _>("accepting_command_id")? {
            accepting_commands.push(
                durable_command_id_from_uuid(command_uuid).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("accepting command identity")
                })?,
            );
        }
    }
    let recorded_commands = require_recorded_batch(connection, &accepting_commands).await?;

    let mut turns = Vec::with_capacity(rows.len());
    let mut turn_configurations = BTreeMap::<TurnId, OriginConfiguration>::new();
    let mut pinned_target_identities = BTreeMap::new();
    let mut required_frontiers = BTreeSet::new();
    let mut required_model_calls = BTreeSet::new();
    let mut named_continuation_gate_calls = BTreeSet::new();
    for row in rows {
        let queued_turn = turn_id_from_uuid(required(&row, "queued_turn_id")?);
        let queued_accepted =
            accepted_input_id_from_uuid(required(&row, "queued_accepted_input_id")?);
        let queued_session = session_id_from_uuid(required(&row, "queued_session_id")?);
        let queued_position = decode_position(&row, "queued_position")?;
        let queued_order = match required::<String>(&row, "priority_kind")?.as_str() {
            "ordinary" => {
                if row
                    .try_get::<Option<Uuid>, _>("interrupt_predecessor_turn_id")?
                    .is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("ordinary queue priority").into(),
                    );
                }
                AcceptedInputQueueOrder::ordinary(queued_position)
            }
            "interrupt_immediately_after" => {
                let predecessor =
                    turn_id_from_uuid(required(&row, "interrupt_predecessor_turn_id")?);
                AcceptedInputQueueOrder::interrupt_immediately_after(queued_position, predecessor)
            }
            value => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "queue priority kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };

        let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
        let accepted_session = session_id_from_uuid(required(&row, "accepted_session_id")?);
        let disposition_kind: String = required(&row, "disposition_kind")?;
        let origin_turn = turn_id_from_uuid(required(&row, "origin_turn_id")?);
        let accepted_source_turn: Option<Uuid> = row.try_get("accepted_source_turn_id")?;

        let lifecycle_turn = turn_id_from_uuid(required(&row, "lifecycle_turn_id")?);
        let lifecycle_session = session_id_from_uuid(required(&row, "lifecycle_session_id")?);
        if queued_accepted != accepted_input
            || queued_turn != origin_turn
            || lifecycle_turn != queued_turn
        {
            return Err(SubmitInputCorruption::Inconsistent(
                "scheduling turn identity correlation",
            )
            .into());
        }

        let accepting_command: Option<Uuid> = row.try_get("accepting_command_id")?;
        let goal_generation: Option<Decimal> = row.try_get("goal_generation")?;
        // A command and a goal generation are not exclusive. A dispatched
        // work turn is bound to the generation it runs under while keeping the
        // submit command that accepted its tagged context, so the command is
        // what reconstitutes the input and the generation rides alongside it.
        let (accepted_lifecycle, origin_delivery, origin_configuration, binding) =
            if let Some(accepting_command) = accepting_command {
                let accepting_command =
                    durable_command_id_from_uuid(accepting_command).map_err(|_| {
                        SubmitInputCorruption::Inconsistent("accepting command identity")
                    })?;
                let recorded = recorded_commands
                    .get(&accepting_command)
                    .ok_or(SubmitInputCorruption::Missing("batched origin receipt"))?;
                match (disposition_kind.as_str(), recorded.result()) {
                    (
                        "origin_of",
                        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)),
                    ) if applied.accepted_input() == accepted_input
                        && applied.session() == accepted_session
                        && applied.turn() == queued_turn
                        && accepted_source_turn
                            == accepted_origin_source_turn(recorded.command().delivery())
                                .map(TurnId::into_uuid) =>
                    {
                        (
                            AcceptedInputLifecycle::new(
                                accepted_input,
                                AcceptedInputDisposition::OriginOf(origin_turn),
                            ),
                            recorded.command().delivery(),
                            applied.origin_configuration().clone(),
                            None,
                        )
                    }
                    (
                        "reclassified_as_turn_origin",
                        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(
                            applied,
                        )),
                    ) if applied.accepted_input() == accepted_input
                        && applied.session() == accepted_session
                        && applied.binding().source_turn().into_uuid()
                            == accepted_source_turn.ok_or(SubmitInputCorruption::Missing(
                                "reclassified source turn",
                            ))? =>
                    {
                        let source_turn = applied.binding().source_turn();
                        let source_configuration =
                            turn_configurations.get(&source_turn).cloned().ok_or(
                                SubmitInputCorruption::Missing("reclassified source configuration"),
                            )?;
                        (
                            AcceptedInputLifecycle::new(
                                accepted_input,
                                AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                                    turn: origin_turn,
                                    reason:
                                        SteeringReclassificationReason::NoSafePointBeforeTerminal,
                                },
                            ),
                            recorded.command().delivery(),
                            source_configuration,
                            Some(applied.binding()),
                        )
                    }
                    ("origin_of" | "reclassified_as_turn_origin", _) => {
                        return Err(SubmitInputCorruption::Inconsistent(
                            "scheduling origin command result",
                        )
                        .into());
                    }
                    (value, _) => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "scheduling accepted-input disposition_kind",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                }
            } else {
                if goal_generation.is_none()
                    || disposition_kind != "origin_of"
                    || accepted_source_turn.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("scheduling goal turn shape").into(),
                    );
                }
                let origin_configuration = decode_goal_origin_configuration(&row, queued_session)?;
                let origin_delivery = DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        origin_configuration.session_defaults_version(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                };
                (
                    AcceptedInputLifecycle::new(
                        accepted_input,
                        AcceptedInputDisposition::OriginOf(origin_turn),
                    ),
                    origin_delivery,
                    origin_configuration,
                    None,
                )
            };
        match binding {
            Some(binding) => require_stored_inherited_configuration(&row, binding.source_turn())?,
            None => require_stored_origin_configuration(&row, &origin_configuration)?,
        }
        if turn_configurations
            .insert(queued_turn, origin_configuration.clone())
            .is_some()
        {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate scheduling configuration").into(),
            );
        }

        let state_kind: String = required(&row, "lifecycle_state_kind")?;
        let lineage_kind: Option<String> = row.try_get("start_lineage_kind")?;
        let predecessor: Option<Uuid> = row.try_get("immediate_predecessor_turn_id")?;
        let starting_frontier: Option<Uuid> = row.try_get("starting_frontier_id")?;
        let terminal_frontier: Option<Uuid> = row.try_get("terminal_frontier_id")?;
        let active_phase: Option<String> = row.try_get("active_phase_kind")?;
        let current_attempt: Option<Uuid> = row.try_get("current_attempt_id")?;
        let pinned_target: Option<Uuid> = row.try_get("pinned_provider_model_identity_id")?;
        let model_identity_boundary_required: bool =
            required(&row, "model_identity_boundary_required")?;
        let recovery_model_call: Option<Uuid> = row.try_get("recovery_model_call_id")?;
        let active_tool_round: Option<Uuid> = row.try_get("active_tool_round_call_id")?;
        let approval_tool_request: Option<Uuid> = row.try_get("approval_tool_request_id")?;
        let recovery_tool_attempt: Option<Uuid> = row.try_get("recovery_tool_attempt_id")?;
        let runner_recovery_runner: Option<Uuid> = row.try_get("runner_recovery_runner_id")?;
        let runner_recovery_revision: Option<Decimal> =
            row.try_get("runner_recovery_placement_revision")?;
        let runner_recovery_tool_attempt: Option<Uuid> =
            row.try_get("runner_recovery_tool_attempt_id")?;
        let child_wait_request: Option<Uuid> = row.try_get("child_wait_request_id")?;
        let terminal_attempt: Option<Uuid> = row.try_get("terminal_attempt_id")?;
        let terminal_model_call: Option<Uuid> = row.try_get("terminal_model_call_id")?;
        let terminal_tool_attempt: Option<Uuid> = row.try_get("terminal_tool_attempt_id")?;
        let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let automatic_reconciliation_model_call: Option<Uuid> =
            row.try_get("automatic_reconciliation_model_call_id")?;
        let automatic_reconciliation_tool_attempt: Option<Uuid> =
            row.try_get("automatic_reconciliation_tool_attempt_id")?;
        let automatic_reconciliation_state: Option<String> =
            row.try_get("automatic_reconciliation_state_kind")?;
        let automatic_reconciliation_attempt_count: Option<i32> =
            row.try_get("automatic_reconciliation_attempt_count")?;
        if active_phase.as_deref() != Some("awaiting_runner_recovery")
            && (runner_recovery_runner.is_some()
                || runner_recovery_revision.is_some()
                || runner_recovery_tool_attempt.is_some())
        {
            return Err(
                SubmitInputCorruption::Inconsistent("runner recovery lifecycle payload").into(),
            );
        }
        let automatic_reconciliation_present = automatic_reconciliation_model_call.is_some()
            || automatic_reconciliation_tool_attempt.is_some()
            || automatic_reconciliation_state.is_some()
            || automatic_reconciliation_attempt_count.is_some();
        if automatic_reconciliation_present
            && !(state_kind == "active"
                && matches!(
                    active_phase.as_deref(),
                    Some("awaiting_model_call_recovery" | "awaiting_tool_recovery")
                )
                || state_kind == "terminal"
                    && terminal_disposition.as_deref() == Some("reconciliation_required"))
        {
            return Err(SubmitInputCorruption::Inconsistent(
                "automatic model-call reconciliation lifecycle phase",
            )
            .into());
        }
        let state = match state_kind.as_str() {
            "queued" => {
                if lineage_kind.is_some()
                    || predecessor.is_some()
                    || starting_frontier.is_some()
                    || terminal_frontier.is_some()
                    || active_phase.is_some()
                    || current_attempt.is_some()
                    || recovery_model_call.is_some()
                    || active_tool_round.is_some()
                    || approval_tool_request.is_some()
                    || recovery_tool_attempt.is_some()
                    || runner_recovery_runner.is_some()
                    || runner_recovery_revision.is_some()
                    || runner_recovery_tool_attempt.is_some()
                    || child_wait_request.is_some()
                    || terminal_attempt.is_some()
                    || terminal_model_call.is_some()
                    || terminal_tool_attempt.is_some()
                    || terminal_disposition.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("queued scheduling lifecycle").into(),
                    );
                }
                AcceptedInputTurnSchedulingRecordState::Queued
            }
            "active" => {
                if terminal_frontier.is_some()
                    || terminal_attempt.is_some()
                    || terminal_model_call.is_some()
                    || terminal_tool_attempt.is_some()
                    || terminal_disposition.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("active scheduling lifecycle").into(),
                    );
                }
                let phase = match active_phase.as_deref() {
                    Some("awaiting_runner_recovery")
                        if current_attempt.is_none()
                            && recovery_model_call.is_none()
                            && approval_tool_request.is_none()
                            && recovery_tool_attempt.is_none()
                            && child_wait_request.is_none() =>
                    {
                        let runner = runner_recovery_runner
                            .ok_or(SubmitInputCorruption::Missing("runner_recovery_runner_id"))?;
                        let revision = runner_recovery_revision.ok_or(
                            SubmitInputCorruption::Missing("runner_recovery_placement_revision"),
                        )?;
                        let revision = positive_u64_from_numeric(revision)
                            .ok()
                            .and_then(RunnerGeneration::try_from_u64)
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "runner recovery placement revision",
                            ))?;
                        let source_snapshot = load_runner_recovery_source_snapshot(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Inconsistent(
                            "runner recovery source snapshot missing",
                        ))?;
                        required_frontiers
                            .insert(source_snapshot.frontier().snapshot().into_uuid());
                        ActiveTurnSchedulingReconstitutionInput::awaiting_runner_recovery(
                            lifecycle_turn,
                            RunnerId::from_uuid(runner),
                            revision,
                            runner_recovery_tool_attempt.map(ToolAttemptId::from_uuid),
                            Some(source_snapshot.frontier().snapshot()),
                        )
                    }
                    Some("running") if recovery_model_call.is_none() => {
                        if approval_tool_request.is_some() || recovery_tool_attempt.is_some() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "running tool phase references",
                            )
                            .into());
                        }
                        let attempt_id = TurnAttemptId::from_uuid(
                            current_attempt
                                .ok_or(SubmitInputCorruption::Missing("current_attempt_id"))?,
                        );
                        require_current_attempt_row(
                            &row,
                            lifecycle_session,
                            lifecycle_turn,
                            attempt_id,
                        )?;
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if end_variant.is_some() || end_disposition.is_some() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "active live attempt end",
                            )
                            .into());
                        }
                        let mut phase = match attempt_state.as_str() {
                            "prepared" => ActiveTurnSchedulingReconstitutionInput::prepared(
                                lifecycle_turn,
                                attempt_id,
                            ),
                            "running" => ActiveTurnSchedulingReconstitutionInput::running(
                                lifecycle_turn,
                                attempt_id,
                            ),
                            "stop_requested" => {
                                let call = required::<Uuid>(&row, "stop_requested_model_call_id")?;
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                required_model_calls.insert(call);
                                ActiveTurnSchedulingReconstitutionInput::stop_requested(
                                    lifecycle_turn,
                                    attempt_id,
                                    ModelCallId::from_uuid(call),
                                    interrupt,
                                )
                            }
                            value => {
                                return Err(SubmitInputCorruption::Unsupported {
                                    field: "active attempt state_kind",
                                    value: value.to_owned(),
                                }
                                .into());
                            }
                        };
                        if let Some(round_call) = active_tool_round {
                            let batch = load_active_batch_from_connection(
                                connection,
                                lifecycle_session,
                                lifecycle_turn,
                            )
                            .await
                            .map_err(map_tool_loop_error)?
                            .ok_or(SubmitInputCorruption::Missing("active tool batch"))?;
                            if batch.producing_call().into_uuid() != round_call
                                || !matches!(
                                    batch.phase(),
                                    signalbox_domain::ToolBatchPhase::Executing {
                                        turn_attempt
                                    } if turn_attempt == attempt_id
                                )
                            {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "running tool batch",
                                )
                                .into());
                            }
                            required_frontiers
                                .insert(batch.yielded_snapshot().frontier().snapshot().into_uuid());
                            required_model_calls.insert(round_call);
                            phase = phase.with_executing_tool_batch(&batch);
                        }
                        phase
                    }
                    Some("awaiting_model_call_recovery")
                        if active_tool_round.is_none()
                            && approval_tool_request.is_none()
                            && recovery_tool_attempt.is_none() =>
                    {
                        let recovery_call = recovery_model_call
                            .ok_or(SubmitInputCorruption::Missing("recovery_model_call_id"))?;
                        let attempt_id = TurnAttemptId::from_uuid(
                            current_attempt
                                .ok_or(SubmitInputCorruption::Missing("current_attempt_id"))?,
                        );
                        require_current_attempt_row(
                            &row,
                            lifecycle_session,
                            lifecycle_turn,
                            attempt_id,
                        )?;
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if attempt_state != "ended" {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "model-call recovery attempt end",
                            )
                            .into());
                        }
                        required_model_calls.insert(recovery_call);
                        named_continuation_gate_calls.insert(recovery_call);
                        match (end_variant.as_deref(), end_disposition.as_deref()) {
                            (Some("without_stop"), Some("ambiguous")) => ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery(
                                lifecycle_turn,
                                attempt_id,
                                ModelCallId::from_uuid(recovery_call),
                            ),
                            (Some("without_stop"), Some("lost")) => ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_restart(
                                lifecycle_turn,
                                attempt_id,
                                ModelCallId::from_uuid(recovery_call),
                            ),
                            (Some("after_cancellation"), Some("ambiguous")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_cancellation(
                                    lifecycle_turn,
                                    attempt_id,
                                    ModelCallId::from_uuid(recovery_call),
                                    interrupt,
                                )
                            }
                            (Some("after_cancellation"), Some("lost")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_cancellation_restart(
                                    lifecycle_turn,
                                    attempt_id,
                                    ModelCallId::from_uuid(recovery_call),
                                    interrupt,
                                )
                            }
                            (None, _) | (_, None) => {
                                return Err(SubmitInputCorruption::Missing(
                                    "model-call recovery attempt end",
                                )
                                .into());
                            }
                            (Some(value), Some(_)) => {
                                return Err(SubmitInputCorruption::Unsupported {
                                    field: "model-call recovery attempt end_variant",
                                    value: value.to_owned(),
                                }
                                .into());
                            }
                        }
                    }
                    Some("awaiting_tool_approval")
                        if recovery_model_call.is_none()
                            && recovery_tool_attempt.is_none()
                            && current_attempt.is_none() =>
                    {
                        let round_call = active_tool_round
                            .ok_or(SubmitInputCorruption::Missing("active_tool_round_call_id"))?;
                        let request = approval_tool_request
                            .ok_or(SubmitInputCorruption::Missing("approval_tool_request_id"))?;
                        if row.try_get::<Option<Uuid>, _>("turn_attempt_id")?.is_some() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "approval wait attempt",
                            )
                            .into());
                        }
                        let batch = load_active_batch_from_connection(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Missing("active tool batch"))?;
                        batch
                            .awaiting_approval()
                            .filter(|wait| wait.request().into_uuid() == request)
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "tool approval wait evidence",
                            ))?;
                        required_frontiers
                            .insert(batch.yielded_snapshot().frontier().snapshot().into_uuid());
                        required_model_calls.insert(round_call);
                        ActiveTurnSchedulingReconstitutionInput::awaiting_approval(
                            lifecycle_turn,
                            &batch,
                        )
                        .ok_or(SubmitInputCorruption::Inconsistent(
                            "tool approval batch evidence",
                        ))?
                    }
                    Some("awaiting_tool_recovery")
                        if recovery_model_call.is_none() && approval_tool_request.is_none() =>
                    {
                        let round_call = active_tool_round
                            .ok_or(SubmitInputCorruption::Missing("active_tool_round_call_id"))?;
                        let recovery_attempt = recovery_tool_attempt
                            .ok_or(SubmitInputCorruption::Missing("recovery_tool_attempt_id"))?;
                        let attempt_id = TurnAttemptId::from_uuid(
                            current_attempt
                                .ok_or(SubmitInputCorruption::Missing("current_attempt_id"))?,
                        );
                        require_current_attempt_row(
                            &row,
                            lifecycle_session,
                            lifecycle_turn,
                            attempt_id,
                        )?;
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if attempt_state != "ended" {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "tool recovery turn-attempt end",
                            )
                            .into());
                        }
                        let batch = load_active_batch_from_connection(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Missing("active tool batch"))?;
                        let wait = batch
                            .awaiting_recovery()
                            .filter(|wait| wait.attempt().into_uuid() == recovery_attempt)
                            .ok_or(SubmitInputCorruption::Inconsistent(
                                "tool recovery wait evidence",
                            ))?;
                        required_model_calls.insert(round_call);
                        required_frontiers.insert(wait.yielded_frontier().into_uuid());
                        match (end_variant.as_deref(), end_disposition.as_deref()) {
                            (Some("without_stop"), Some("ambiguous")) => {
                                ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery(
                                    lifecycle_turn,
                                    attempt_id,
                                    wait,
                                )
                            }
                            (Some("without_stop"), Some("lost")) => {
                                ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery_after_restart(
                                    lifecycle_turn,
                                    attempt_id,
                                    wait,
                                )
                            }
                            (Some("after_cancellation"), Some("ambiguous")) => {
                                ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery_after_cancellation(
                                    lifecycle_turn,
                                    attempt_id,
                                    wait,
                                    require_applied_interrupt_from_attempt(
                                        &row,
                                        lifecycle_turn,
                                        &recorded_commands,
                                    )?,
                                )
                            }
                            (Some("after_cancellation"), Some("lost")) => {
                                ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery_after_cancellation_restart(
                                    lifecycle_turn,
                                    attempt_id,
                                    wait,
                                    require_applied_interrupt_from_attempt(
                                        &row,
                                        lifecycle_turn,
                                        &recorded_commands,
                                    )?,
                                )
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "tool recovery attempt end",
                                )
                                .into());
                            }
                        }
                    }
                    Some("awaiting_child")
                        if current_attempt.is_none()
                            && recovery_model_call.is_none()
                            && approval_tool_request.is_none()
                            && recovery_tool_attempt.is_none() =>
                    {
                        let round_call = active_tool_round
                            .ok_or(SubmitInputCorruption::Missing("active_tool_round_call_id"))?;
                        let awaiting_request = child_wait_request
                            .ok_or(SubmitInputCorruption::Missing("child_wait_request_id"))?;
                        let batch = load_active_batch_from_connection(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                        )
                        .await
                        .map_err(map_tool_loop_error)?
                        .ok_or(SubmitInputCorruption::Missing("active tool batch"))?;
                        if batch.producing_call().into_uuid() != round_call
                            || !matches!(
                                batch.phase(),
                                signalbox_domain::ToolBatchPhase::AwaitingChild { request, .. }
                                    if request.into_uuid() == awaiting_request
                            )
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "foreground child wait evidence",
                            )
                            .into());
                        }
                        required_frontiers
                            .insert(batch.yielded_snapshot().frontier().snapshot().into_uuid());
                        required_model_calls.insert(round_call);
                        ActiveTurnSchedulingReconstitutionInput::awaiting_child(
                            lifecycle_turn,
                            &batch,
                        )
                        .ok_or(SubmitInputCorruption::Inconsistent(
                            "foreground child wait batch evidence",
                        ))?
                    }
                    Some(value) => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "active phase kind",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                    None => {
                        return Err(SubmitInputCorruption::Missing("active_phase_kind").into());
                    }
                };
                let starting_frontier = starting_frontier
                    .ok_or(SubmitInputCorruption::Missing("starting_frontier_id"))?;
                required_frontiers.insert(starting_frontier);
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: decode_starting_lineage(lineage_kind, predecessor)?,
                    starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                    phase,
                }
            }
            "terminal" => {
                if active_phase.is_some()
                    || current_attempt.is_some()
                    || recovery_model_call.is_some()
                    || active_tool_round.is_some()
                    || approval_tool_request.is_some()
                    || recovery_tool_attempt.is_some()
                    || child_wait_request.is_some()
                {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "terminal scheduling lifecycle",
                    )
                    .into());
                }
                let starting_frontier = starting_frontier
                    .ok_or(SubmitInputCorruption::Missing("starting_frontier_id"))?;
                let terminal_frontier = terminal_frontier
                    .ok_or(SubmitInputCorruption::Missing("terminal_frontier_id"))?;
                required_frontiers.insert(starting_frontier);
                required_frontiers.insert(terminal_frontier);
                let starting_lineage = decode_starting_lineage(lineage_kind, predecessor)?;
                if terminal_tool_attempt.is_some()
                    && terminal_disposition.as_deref() != Some("reconciliation_required")
                {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "terminal tool attempt disposition",
                    )
                    .into());
                }
                match terminal_disposition.as_deref() {
                    Some("failed") => {
                        let terminal_execution = match (terminal_attempt, terminal_model_call) {
                            (None, None) => None,
                            (Some(terminal_attempt), terminal_call) => {
                                let stored_attempt_id =
                                    TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                                let attempt_turn =
                                    turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                                let attempt_session =
                                    session_id_from_uuid(required(&row, "attempt_session_id")?);
                                let attempt_state: String = required(&row, "attempt_state_kind")?;
                                let end_variant: Option<String> = row.try_get("end_variant")?;
                                let end_disposition: Option<String> =
                                    row.try_get("end_disposition")?;
                                if stored_attempt_id.into_uuid() != terminal_attempt
                                    || attempt_turn != lifecycle_turn
                                    || attempt_session != lifecycle_session
                                    || attempt_state != "ended"
                                {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "failed terminal attempt",
                                    )
                                    .into());
                                }
                                let ended_call = terminal_call.map(|call| {
                                    required_model_calls.insert(call);
                                    ModelCallId::from_uuid(call)
                                });
                                // A failed turn's terminal frontier can close
                                // a tool round whether the stored provenance
                                // names no call (a round loss or a
                                // denial-only closure) or the round's
                                // continuation call (a provider failure or a
                                // lost prepared call at the continuation
                                // boundary), so the result evidence loads for
                                // both shapes.
                                let failed_terminal_frontier =
                                    ContextFrontierId::from_uuid(terminal_frontier);
                                let terminal_tool_attempts = load_terminal_result_attempts(
                                    connection,
                                    lifecycle_session,
                                    lifecycle_turn,
                                    failed_terminal_frontier,
                                )
                                .await
                                .map_err(map_tool_loop_error)?;
                                let terminal_tool_denials = load_terminal_result_denials(
                                    connection,
                                    lifecycle_session,
                                    lifecycle_turn,
                                    failed_terminal_frontier,
                                )
                                .await
                                .map_err(map_tool_loop_error)?;
                                let execution = match (
                                    end_variant.as_deref(),
                                    end_disposition.as_deref(),
                                    ended_call,
                                ) {
                                    (Some("without_stop"), Some("known_failure"), Some(call)) => {
                                        FailedTurnExecutionReconstitutionInput::with_call(
                                            lifecycle_turn,
                                            stored_attempt_id,
                                            UnstoppedAttemptDisposition::KnownFailure,
                                            call,
                                        )
                                    }
                                    (Some("without_stop"), Some("known_failure"), None) => {
                                        FailedTurnExecutionReconstitutionInput::attempt_only(
                                            lifecycle_turn,
                                            stored_attempt_id,
                                            UnstoppedAttemptDisposition::KnownFailure,
                                        )
                                    }
                                    (Some("without_stop"), Some("lost"), Some(call)) => {
                                        FailedTurnExecutionReconstitutionInput::with_call(
                                            lifecycle_turn,
                                            stored_attempt_id,
                                            UnstoppedAttemptDisposition::Lost,
                                            call,
                                        )
                                    }
                                    (Some("without_stop"), Some("lost"), None) => {
                                        FailedTurnExecutionReconstitutionInput::attempt_only(
                                            lifecycle_turn,
                                            stored_attempt_id,
                                            UnstoppedAttemptDisposition::Lost,
                                        )
                                    }
                                    (
                                        Some("after_cancellation"),
                                        Some("known_failure"),
                                        ended_call,
                                    ) => {
                                        let interrupt = require_applied_interrupt_from_attempt(
                                            &row,
                                            lifecycle_turn,
                                            &recorded_commands,
                                        )?;
                                        match ended_call {
                                            Some(call) => FailedTurnExecutionReconstitutionInput::with_call_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::KnownFailure,
                                                interrupt,
                                                call,
                                            ),
                                            None => FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::KnownFailure,
                                                interrupt,
                                            ),
                                        }
                                    }
                                    (Some("after_cancellation"), Some("lost"), ended_call) => {
                                        let interrupt = require_applied_interrupt_from_attempt(
                                            &row,
                                            lifecycle_turn,
                                            &recorded_commands,
                                        )?;
                                        match ended_call {
                                            Some(call) => FailedTurnExecutionReconstitutionInput::with_call_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::Lost,
                                                interrupt,
                                                call,
                                            ),
                                            None => FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::Lost,
                                                interrupt,
                                            ),
                                        }
                                    }
                                    _ => {
                                        return Err(SubmitInputCorruption::Inconsistent(
                                            "failed terminal attempt disposition",
                                        )
                                        .into());
                                    }
                                };
                                Some(
                                    execution
                                        .with_terminal_tool_attempts(terminal_tool_attempts)
                                        .with_terminal_tool_denials(terminal_tool_denials),
                                )
                            }
                            (None, Some(_)) => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "failed terminal call without attempt",
                                )
                                .into());
                            }
                        };
                        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                            starting_lineage,
                            starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                            terminal_execution,
                            terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                        }
                    }
                    Some("cancelled") => {
                        let terminal_attempt = terminal_attempt
                            .ok_or(SubmitInputCorruption::Missing("terminal_attempt_id"))?;
                        let stored_attempt_id =
                            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                        let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                        let attempt_session =
                            session_id_from_uuid(required(&row, "attempt_session_id")?);
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if stored_attempt_id.into_uuid() != terminal_attempt
                            || attempt_turn != lifecycle_turn
                            || attempt_session != lifecycle_session
                            || attempt_state != "ended"
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "cancelled terminal attempt",
                            )
                            .into());
                        }
                        let (attempt_end, interrupt) = match (
                            end_variant.as_deref(),
                            end_disposition.as_deref(),
                        ) {
                            (Some("after_cancellation"), Some("cancelled")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                (
                                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                                        CancellationStopDisposition::Cancelled,
                                        interrupt,
                                    ),
                                    interrupt,
                                )
                            }
                            (Some("without_stop"), Some("yielded_to_durable_wait")) => {
                                let interrupted_tool_attempt =
                                    row.try_get("runner_recovery_interrupted_tool_attempt_id")?;
                                let interrupt = require_applied_runner_recovery_interrupt(
                                    &row,
                                    lifecycle_turn,
                                    stored_attempt_id,
                                    interrupted_tool_attempt,
                                    &recorded_commands,
                                )?;
                                (
                                    TerminalAttemptEndReconstitutionInput::yielded_to_runner_recovery(
                                        interrupt,
                                    ),
                                    interrupt,
                                )
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "cancelled terminal attempt disposition",
                                )
                                .into());
                            }
                        };
                        let ended_call = terminal_model_call.map(ModelCallId::from_uuid);
                        if let Some(call) = terminal_model_call {
                            required_model_calls.insert(call);
                        }
                        // A cancelled turn's terminal frontier can close a tool
                        // round whether the stored provenance names no call (a
                        // batch interrupt) or the round's completed producing
                        // call (a stop racing a tool-using response), so the
                        // result evidence loads for both shapes.
                        let cancelled_terminal_frontier =
                            ContextFrontierId::from_uuid(terminal_frontier);
                        let terminal_tool_attempts = load_terminal_result_attempts(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                            cancelled_terminal_frontier,
                        )
                        .await
                        .map_err(map_tool_loop_error)?;
                        let terminal_tool_denials = load_terminal_result_denials(
                            connection,
                            lifecycle_session,
                            lifecycle_turn,
                            cancelled_terminal_frontier,
                        )
                        .await
                        .map_err(map_tool_loop_error)?;
                        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                            starting_lineage,
                            starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                                lifecycle_turn,
                                stored_attempt_id,
                                attempt_end,
                                ended_call,
                                interrupt,
                            )
                            .with_terminal_tool_attempts(terminal_tool_attempts)
                            .with_terminal_tool_denials(terminal_tool_denials),
                            terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                        }
                    }
                    Some("reconciliation_required") => {
                        let terminal_attempt = terminal_attempt
                            .ok_or(SubmitInputCorruption::Missing("terminal_attempt_id"))?;
                        let stored_attempt_id =
                            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                        let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                        let attempt_session =
                            session_id_from_uuid(required(&row, "attempt_session_id")?);
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if stored_attempt_id.into_uuid() != terminal_attempt
                            || attempt_turn != lifecycle_turn
                            || attempt_session != lifecycle_session
                            || attempt_state != "ended"
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "reconciliation terminal attempt",
                            )
                            .into());
                        }
                        let automatic_authority = match (
                            automatic_reconciliation_model_call,
                            automatic_reconciliation_tool_attempt,
                            automatic_reconciliation_state.as_deref(),
                            automatic_reconciliation_attempt_count,
                        ) {
                            (None, None, None, None) => None,
                            (model_call, tool_attempt, Some("reconciled"), Some(attempts))
                                if model_call == terminal_model_call
                                    && tool_attempt == terminal_tool_attempt
                                    && model_call.is_some() != tool_attempt.is_some() =>
                            {
                                let attempt = u32::try_from(attempts)
                                    .ok()
                                    .and_then(NonZeroU32::new)
                                    .filter(|attempt| attempt.get() <= 5)
                                    .ok_or(SubmitInputCorruption::Inconsistent(
                                        "automatic model-call reconciliation attempt",
                                    ))?;
                                Some(AutomaticReconciliationAuthority::AutomaticRecovery {
                                    attempt,
                                })
                            }
                            (model_call, tool_attempt, Some("superseded"), Some(attempts))
                                if model_call == terminal_model_call
                                    && tool_attempt == terminal_tool_attempt
                                    && model_call.is_some() != tool_attempt.is_some()
                                    && u32::try_from(attempts)
                                        .is_ok_and(|attempts| attempts <= 5) =>
                            {
                                None
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "automatic model-call reconciliation authority",
                                )
                                .into());
                            }
                        };
                        let (authority, reconciling_attempt_end) = match (
                            end_variant.as_deref(),
                            end_disposition.as_deref(),
                        ) {
                            (Some("without_stop"), Some(disposition @ ("ambiguous" | "lost"))) => {
                                let interrupt =
                                    applied_interrupt_for_turn(lifecycle_turn, &recorded_commands)?;
                                let authority = match (interrupt, automatic_authority) {
                                    (Some(interrupt), _) => {
                                        AutomaticReconciliationAuthority::AppliedInterrupt(
                                            interrupt,
                                        )
                                    }
                                    (None, Some(authority)) => authority,
                                    (None, None) => {
                                        return Err(SubmitInputCorruption::Missing(
                                            "model-call reconciliation authority",
                                        )
                                        .into());
                                    }
                                };
                                let attempt_end = if disposition == "ambiguous" {
                                    TerminalAttemptEndReconstitutionInput::without_stop(
                                        UnstoppedAttemptDisposition::Ambiguous,
                                    )
                                } else {
                                    TerminalAttemptEndReconstitutionInput::without_stop(
                                        UnstoppedAttemptDisposition::Lost,
                                    )
                                };
                                (authority, attempt_end)
                            }
                            (Some("after_cancellation"), Some("ambiguous")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                (
                                    AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
                                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                                        CancellationStopDisposition::Ambiguous,
                                        interrupt,
                                    ),
                                )
                            }
                            (Some("after_cancellation"), Some("lost")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                (
                                    AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
                                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                                        CancellationStopDisposition::Lost,
                                        interrupt,
                                    ),
                                )
                            }
                            (Some("without_stop"), Some("yielded_to_durable_wait")) => {
                                if automatic_authority.is_some() {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "automatic authority with runner recovery attempt",
                                    )
                                    .into());
                                }
                                let interrupt = require_applied_runner_recovery_interrupt(
                                    &row,
                                    lifecycle_turn,
                                    stored_attempt_id,
                                    terminal_tool_attempt,
                                    &recorded_commands,
                                )?;
                                (
                                    AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
                                    TerminalAttemptEndReconstitutionInput::yielded_to_runner_recovery(
                                        interrupt,
                                    ),
                                )
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "reconciliation terminal attempt disposition",
                                )
                                .into());
                            }
                        };
                        match (terminal_model_call, terminal_tool_attempt) {
                            (Some(terminal_call), None) => {
                                required_model_calls.insert(terminal_call);
                                named_continuation_gate_calls.insert(terminal_call);
                                AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                                    starting_lineage,
                                    starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                                    reconciling_attempt: stored_attempt_id,
                                    reconciling_attempt_end,
                                    ambiguous_call: ModelCallId::from_uuid(terminal_call),
                                    authority,
                                    terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                                }
                            }
                            (None, Some(terminal_tool_attempt)) => {
                                let batch = load_recovery_batch_by_attempt(
                                    connection,
                                    lifecycle_session,
                                    lifecycle_turn,
                                    signalbox_domain::ToolAttemptId::from_uuid(
                                        terminal_tool_attempt,
                                    ),
                                )
                                .await
                                .map_err(map_tool_loop_error)?;
                                let ambiguous_tool = batch.awaiting_recovery().ok_or(
                                    SubmitInputCorruption::Inconsistent(
                                        "terminal tool recovery evidence",
                                    ),
                                )?;
                                if ambiguous_tool.issuing_attempt() != stored_attempt_id {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "terminal tool recovery turn attempt",
                                    )
                                    .into());
                                }
                                required_model_calls
                                    .insert(ambiguous_tool.producing_call().into_uuid());
                                required_frontiers
                                    .insert(ambiguous_tool.yielded_frontier().into_uuid());
                                AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                                    starting_lineage,
                                    starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                                    reconciling_attempt: stored_attempt_id,
                                    reconciling_attempt_end,
                                    tool_batch: batch,
                                    authority,
                                    terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                                }
                            }
                            (Some(_), Some(_)) | (None, None) => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "reconciliation terminal operation",
                                )
                                .into());
                            }
                        }
                    }
                    Some("completed" | "refused") => {
                        let terminal_attempt = terminal_attempt
                            .ok_or(SubmitInputCorruption::Missing("terminal_attempt_id"))?;
                        let terminal_call = terminal_model_call
                            .ok_or(SubmitInputCorruption::Missing("terminal_model_call_id"))?;
                        let stored_attempt_id =
                            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                        let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                        let attempt_session =
                            session_id_from_uuid(required(&row, "attempt_session_id")?);
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if stored_attempt_id.into_uuid() != terminal_attempt
                            || attempt_turn != lifecycle_turn
                            || attempt_session != lifecycle_session
                            || attempt_state != "ended"
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "terminal model-call attempt",
                            )
                            .into());
                        }
                        required_model_calls.insert(terminal_call);
                        match terminal_disposition.as_deref() {
                            Some("completed") => {
                                let completing_attempt_end = match (
                                    end_variant.as_deref(),
                                    end_disposition.as_deref(),
                                ) {
                                    (Some("without_stop"), Some("turn_completed")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::TurnCompleted,
                                        )
                                    }
                                    (Some("without_stop"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::Lost,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("turn_completed")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::TurnCompleted,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::Lost,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    _ => {
                                        return Err(SubmitInputCorruption::Inconsistent(
                                            "terminal model-call disposition",
                                        )
                                        .into());
                                    }
                                };
                                AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                                    starting_lineage,
                                    starting_frontier: ContextFrontierId::from_uuid(
                                        starting_frontier,
                                    ),
                                    completing_attempt: stored_attempt_id,
                                    completing_attempt_end,
                                    completing_call: ModelCallId::from_uuid(terminal_call),
                                    terminal_frontier: ContextFrontierId::from_uuid(
                                        terminal_frontier,
                                    ),
                                }
                            }
                            Some("refused") => {
                                let refusing_attempt_end = match (
                                    end_variant.as_deref(),
                                    end_disposition.as_deref(),
                                ) {
                                    (Some("without_stop"), Some("turn_refused")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::TurnRefused,
                                        )
                                    }
                                    (Some("without_stop"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::Lost,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("turn_refused")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::TurnRefused,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::Lost,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    _ => {
                                        return Err(SubmitInputCorruption::Inconsistent(
                                            "terminal model-call disposition",
                                        )
                                        .into());
                                    }
                                };
                                named_continuation_gate_calls.insert(terminal_call);
                                AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                                    starting_lineage,
                                    starting_frontier: ContextFrontierId::from_uuid(
                                        starting_frontier,
                                    ),
                                    refusing_attempt: stored_attempt_id,
                                    refusing_attempt_end,
                                    refusing_call: ModelCallId::from_uuid(terminal_call),
                                    terminal_frontier: ContextFrontierId::from_uuid(
                                        terminal_frontier,
                                    ),
                                }
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "terminal model-call disposition",
                                )
                                .into());
                            }
                        }
                    }
                    Some(value) => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "terminal disposition kind",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                    None => {
                        return Err(
                            SubmitInputCorruption::Missing("terminal_disposition_kind").into()
                        );
                    }
                }
            }
            value => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "turn lifecycle state_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };

        if let Some(identity) = pinned_target
            && pinned_target_identities
                .insert(queued_turn, identity)
                .is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("duplicate turn target pin").into());
        }

        let mut record = match binding {
            Some(binding) => AcceptedInputTurnSchedulingRecord::reclassified(
                lifecycle_session,
                lifecycle_turn,
                accepted_session,
                accepted_lifecycle,
                queued_session,
                queued_turn,
                AcceptedInputQueueOrder::ordinary(queued_position),
                origin_delivery,
                binding,
                origin_configuration,
                state,
            ),
            None => AcceptedInputTurnSchedulingRecord::new(
                lifecycle_session,
                lifecycle_turn,
                accepted_session,
                accepted_lifecycle,
                queued_session,
                queued_turn,
                queued_order,
                origin_delivery,
                origin_configuration,
                state,
            ),
        };
        if !model_identity_boundary_required {
            record = record.without_legacy_model_identity_boundary();
        }
        turns.push(record);
    }

    let active_acceptance_tail =
        load_active_acceptance_tail(connection, session_id, &turns).await?;

    let consumed_steering_rows = sqlx::query(
        "SELECT accepted.session_id, accepted.accepted_input_id,
                accepted.acceptance_position, accepted.expected_active_turn_id,
                accepted.consuming_model_call_id
           FROM accepted_input AS accepted
           JOIN turn_lifecycle AS source
             ON source.turn_id = accepted.expected_active_turn_id
            AND source.session_id = accepted.session_id
            AND source.origin_kind = 'accepted_input'
          WHERE accepted.session_id = $1
            AND accepted.disposition_kind = 'consumed_as_steering'
          ORDER BY accepted.acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut consumed_steering = Vec::with_capacity(consumed_steering_rows.len());
    let mut consumed_counts_by_call = BTreeMap::<Uuid, u64>::new();
    for row in consumed_steering_rows {
        let call = ModelCallId::from_uuid(required(&row, "consuming_model_call_id")?);
        required_model_calls.insert(call.into_uuid());
        *consumed_counts_by_call.entry(call.into_uuid()).or_default() += 1;
        consumed_steering.push(ConsumedSteeringReconstitutionInput::new(
            session_id_from_uuid(required(&row, "session_id")?),
            AcceptedInputLifecycle::new(
                accepted_input_id_from_uuid(required(&row, "accepted_input_id")?),
                AcceptedInputDisposition::ConsumedAsSteering { call },
            ),
            decode_position(&row, "acceptance_position")?,
            turn_id_from_uuid(required(&row, "expected_active_turn_id")?),
        ));
    }
    let delegated_consumed_steering_rows = sqlx::query(
        "SELECT accepted.session_id, accepted.accepted_input_id,
                accepted.acceptance_position, accepted.expected_active_turn_id,
                accepted.consuming_model_call_id
           FROM accepted_input AS accepted
           JOIN turn_lifecycle AS source
             ON source.turn_id = accepted.expected_active_turn_id
            AND source.session_id = accepted.session_id
            AND source.origin_kind = 'delegation'
          WHERE accepted.session_id = $1
            AND accepted.disposition_kind = 'consumed_as_steering'
          ORDER BY accepted.acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut delegated_consumed_steering =
        Vec::with_capacity(delegated_consumed_steering_rows.len());
    for row in delegated_consumed_steering_rows {
        let call = ModelCallId::from_uuid(required(&row, "consuming_model_call_id")?);
        delegated_consumed_steering.push(ConsumedSteeringReconstitutionInput::new(
            session_id_from_uuid(required(&row, "session_id")?),
            AcceptedInputLifecycle::new(
                accepted_input_id_from_uuid(required(&row, "accepted_input_id")?),
                AcceptedInputDisposition::ConsumedAsSteering { call },
            ),
            decode_position(&row, "acceptance_position")?,
            turn_id_from_uuid(required(&row, "expected_active_turn_id")?),
        ));
    }

    let assistant_model_calls = sqlx::query_scalar::<_, Uuid>(
        "SELECT DISTINCT producing_model_call_id
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND payload_kind IN ('assistant_text', 'provider_compaction', 'provider_reasoning', 'assistant_tool_use')
          ORDER BY producing_model_call_id",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    required_model_calls.extend(assistant_model_calls);

    let required_model_call_ids = required_model_calls.iter().copied().collect::<Vec<_>>();
    let model_call_rows = sqlx::query(
        "SELECT
            call.model_call_id,
            call.turn_id,
            call.session_id,
            call.turn_attempt_id,
            call.selection_kind,
            call.direct_model_selection_id,
            call.frozen_model_alias_id,
            call.frozen_alias_selected_direct_id,
            call.resolved_provider_model_identity_id,
            call.context_frontier_id,
            call.state_kind,
            call.terminal_disposition_kind,
            manifest.turn_instruction_manifest_id,
            manifest.boundary_kind AS instruction_manifest_boundary_kind,
            manifest.eligibility_hash_algorithm
                AS instruction_eligibility_hash_algorithm,
            manifest.eligibility_hash AS instruction_eligibility_hash,
            manifest.admitted_set_hash_algorithm
                AS instruction_admitted_set_hash_algorithm,
            manifest.admitted_set_hash AS instruction_admitted_set_hash,
            manifest.manifest_hash_algorithm
                AS instruction_manifest_hash_algorithm,
            manifest.manifest_hash AS instruction_manifest_hash,
            discovery.scan_complete AS instruction_discovery_complete,
            lifecycle.origin_kind AS turn_origin_kind,
            lifecycle.pinned_provider_model_identity_id,
            (attempt.continued_from_attempt_id IS NOT NULL)
                AS continues_prior_attempt
           FROM model_call AS call
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = call.turn_attempt_id
            AND attempt.turn_id = call.turn_id
            AND attempt.session_id = call.session_id
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = call.turn_id
            AND lifecycle.session_id = call.session_id
      LEFT JOIN turn_instruction_manifest AS manifest
             ON manifest.turn_instruction_manifest_id = call.turn_instruction_manifest_id
            AND manifest.session_id = call.session_id
            AND manifest.turn_id = call.turn_id
      LEFT JOIN instruction_discovery AS discovery
             ON discovery.instruction_discovery_id = manifest.instruction_discovery_id
          WHERE call.session_id = $1
            AND call.model_call_id = ANY($2)
          ORDER BY call.model_call_id",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&required_model_call_ids)
    .fetch_all(&mut *connection)
    .await?;
    /// One steering-consuming call whose round evidence must be looked up.
    struct ConsumingCallFacts {
        call: Uuid,
        turn: TurnId,
        call_frontier: Uuid,
        consumed_count: u64,
    }
    /// One gate-named steering-free call whose round evidence must be looked
    /// up.
    struct NamedGateCallFacts {
        call: Uuid,
        turn: TurnId,
        call_frontier: Uuid,
    }
    let mut model_calls = Vec::with_capacity(model_call_rows.len());
    let mut pinned_targets = Vec::with_capacity(model_call_rows.len());
    let mut loaded_pinned_turns = BTreeSet::new();
    let mut loaded_model_calls = BTreeSet::new();
    let mut delegated_turns = BTreeSet::new();
    let mut consuming_call_facts = Vec::new();
    let mut named_gate_call_facts = Vec::new();
    for row in model_call_rows {
        let call_uuid: Uuid = required(&row, "model_call_id")?;
        if !loaded_model_calls.insert(call_uuid) {
            return Err(SubmitInputCorruption::Inconsistent("duplicate model call").into());
        }
        let frontier_uuid: Uuid = required(&row, "context_frontier_id")?;
        let turn_uuid: Uuid = required(&row, "turn_id")?;
        let turn = turn_id_from_uuid(turn_uuid);
        crate::model_execution::authenticate_model_call_instruction_manifest(
            &row, session_id, turn,
        )?;
        let turn_origin_kind: String = required(&row, "turn_origin_kind")?;
        if turn_origin_kind == "delegation" {
            delegated_turns.insert(turn);
        }
        let pinned_identity = match pinned_target_identities.get(&turn).copied() {
            Some(identity) => identity,
            None if turn_origin_kind == "delegation" => {
                required(&row, "pinned_provider_model_identity_id")?
            }
            None => {
                return Err(SubmitInputCorruption::Missing("model call turn target pin").into());
            }
        };
        if loaded_pinned_turns.insert(turn) {
            pinned_targets.push(PinnedProviderTargetReconstitutionInput::new(
                turn,
                ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(pinned_identity)),
            ));
        }
        required_frontiers.insert(frontier_uuid);
        let state_kind: String = required(&row, "state_kind")?;
        let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let state = match (state_kind.as_str(), terminal_disposition.as_deref()) {
            ("prepared", None) => ModelCallReconstitutionState::Prepared,
            ("in_flight", None) => ModelCallReconstitutionState::InFlight,
            ("cancellation_requested", None) => ModelCallReconstitutionState::CancellationRequested,
            ("terminal", Some(disposition)) => {
                ModelCallReconstitutionState::Terminal(decode_model_call_disposition(disposition)?)
            }
            ("prepared" | "in_flight" | "cancellation_requested" | "terminal", _) => {
                return Err(SubmitInputCorruption::Inconsistent("model call state payload").into());
            }
            (value, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "model call state_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        // Only a continuation-chain attempt's call can carry a continuation
        // round window, so first-round consumers skip the evidence lookup.
        let continues_prior_attempt: bool = required(&row, "continues_prior_attempt")?;
        if let Some(consumed_count) = consumed_counts_by_call.get(&call_uuid).copied()
            && continues_prior_attempt
        {
            consuming_call_facts.push(ConsumingCallFacts {
                call: call_uuid,
                turn,
                call_frontier: frontier_uuid,
                consumed_count,
            });
        }
        if named_continuation_gate_calls.contains(&call_uuid) && continues_prior_attempt {
            named_gate_call_facts.push(NamedGateCallFacts {
                call: call_uuid,
                turn,
                call_frontier: frontier_uuid,
            });
        }
        model_calls.push(ModelCallReconstitutionInput::new(
            ModelCallId::from_uuid(call_uuid),
            turn,
            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?),
            decode_frozen_model(
                required(&row, "selection_kind")?,
                row.try_get("direct_model_selection_id")?,
                row.try_get("frozen_model_alias_id")?,
                row.try_get("frozen_alias_selected_direct_id")?,
            )?,
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
                &row,
                "resolved_provider_model_identity_id",
            )?)),
            ContextFrontierId::from_uuid(frontier_uuid),
            state,
        ));
    }
    if loaded_model_calls != required_model_calls {
        return Err(SubmitInputCorruption::Missing("scheduling model call").into());
    }

    // A steering-consuming call prepared at a tool-round continuation
    // boundary reconstitutes only together with its round's complete result
    // evidence; a call prepared against its turn's starting frontier has no
    // continuing round window and carries none.
    let mut steering_continuation_rounds = Vec::new();
    for facts in consuming_call_facts {
        let Some((round_tool_attempts, round_tool_denials)) =
            load_steering_continuation_round_evidence(
                connection,
                session_id,
                facts.turn,
                ContextFrontierId::from_uuid(facts.call_frontier),
                facts.consumed_count,
            )
            .await
            .map_err(map_tool_loop_error)?
        else {
            continue;
        };
        steering_continuation_rounds.push(SteeringContinuationRoundReconstitutionInput::new(
            ModelCallId::from_uuid(facts.call),
            round_tool_attempts,
            round_tool_denials,
        ));
    }

    // A steering-free continuation call named by a terminal or recovery gate
    // reconstitutes only together with its round's complete result evidence;
    // a named call prepared against its turn's starting frontier has no
    // continuing round window and carries none.
    let mut continuation_rounds = Vec::new();
    for facts in named_gate_call_facts {
        let Some((round_tool_attempts, round_tool_denials)) = load_continuation_round_evidence(
            connection,
            session_id,
            facts.turn,
            ContextFrontierId::from_uuid(facts.call_frontier),
        )
        .await
        .map_err(map_tool_loop_error)?
        else {
            continue;
        };
        continuation_rounds.push(ContinuationRoundReconstitutionInput::new(
            ModelCallId::from_uuid(facts.call),
            round_tool_attempts,
            round_tool_denials,
        ));
    }

    let compaction_rows = sqlx::query(
        "SELECT
            call.model_call_id,
            call.source_frontier_id AS call_source_frontier_id,
            call.direct_model_selection_id,
            call.resolved_provider_model_identity_id,
            call.state_kind,
            call.terminal_disposition_kind,
            call.input_tokens,
            call.output_tokens,
            call.cache_creation_input_tokens,
            call.cache_read_input_tokens,
            compaction.context_compaction_id,
            compaction.predecessor_compaction_id,
            compaction.source_frontier_id AS compaction_source_frontier_id,
            compaction.result_frontier_id,
            compaction.producing_call_id,
            compaction.first_source_session_id,
            compaction.first_entry_id,
            compaction.through_source_session_id,
            compaction.through_entry_id,
            compaction.summary_entry_id
         FROM context_compaction_model_call AS call
         LEFT JOIN context_compaction AS compaction
           ON compaction.producing_call_id = call.model_call_id
          AND compaction.session_id = call.session_id
        WHERE call.session_id = $1
        ORDER BY call.model_call_id",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut compaction_calls = Vec::with_capacity(compaction_rows.len());
    let mut compactions = Vec::with_capacity(compaction_rows.len());
    for row in compaction_rows {
        let call_id = ModelCallId::from_uuid(required(&row, "model_call_id")?);
        let call_source_frontier_uuid: Uuid = required(&row, "call_source_frontier_id")?;
        required_frontiers.insert(call_source_frontier_uuid);
        let state_kind: String = required(&row, "state_kind")?;
        let disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let state = match (state_kind.as_str(), disposition.as_deref()) {
            ("prepared", None) => ContextCompactionModelCallState::Prepared,
            ("in_flight", None) => ContextCompactionModelCallState::InFlight,
            ("terminal", Some(value)) => {
                ContextCompactionModelCallState::Terminal(decode_model_call_disposition(value)?)
            }
            ("prepared" | "in_flight" | "terminal", _) => {
                return Err(SubmitInputCorruption::Inconsistent(
                    "compaction model call state payload",
                )
                .into());
            }
            (value, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "compaction model call state_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        let usage = ContextCompactionTokenUsage::unreported()
            .with_input_tokens(decode_optional_token_count(&row, "input_tokens")?)
            .with_output_tokens(decode_optional_token_count(&row, "output_tokens")?)
            .with_cache_creation_input_tokens(decode_optional_token_count(
                &row,
                "cache_creation_input_tokens",
            )?)
            .with_cache_read_input_tokens(decode_optional_token_count(
                &row,
                "cache_read_input_tokens",
            )?);
        compaction_calls.push(ContextCompactionModelCallReconstitutionInput::new(
            call_id,
            session_id,
            DirectModelSelection::from_uuid(required(&row, "direct_model_selection_id")?),
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
                &row,
                "resolved_provider_model_identity_id",
            )?)),
            ContextFrontierId::from_uuid(call_source_frontier_uuid),
            state,
            usage,
        ));
        let Some(compaction_uuid) = row.try_get::<Option<Uuid>, _>("context_compaction_id")? else {
            continue;
        };
        let compaction_id = ContextCompactionId::from_uuid(compaction_uuid);
        let producing_call = ModelCallId::from_uuid(required(&row, "producing_call_id")?);
        let source_frontier_uuid: Uuid = required(&row, "compaction_source_frontier_id")?;
        let result_frontier_uuid: Uuid = required(&row, "result_frontier_id")?;
        required_frontiers.insert(source_frontier_uuid);
        required_frontiers.insert(result_frontier_uuid);
        let first = SemanticTranscriptEntryRef::from_source(
            session_id_from_uuid(required(&row, "first_source_session_id")?),
            SemanticTranscriptEntryId::from_uuid(required(&row, "first_entry_id")?),
        );
        let through = SemanticTranscriptEntryRef::from_source(
            session_id_from_uuid(required(&row, "through_source_session_id")?),
            SemanticTranscriptEntryId::from_uuid(required(&row, "through_entry_id")?),
        );
        let predecessor: Option<Uuid> = row.try_get("predecessor_compaction_id")?;
        compactions.push(ContextCompactionReconstitutionInput::new(
            compaction_id,
            session_id,
            predecessor.map(ContextCompactionId::from_uuid),
            ContextFrontierId::from_uuid(source_frontier_uuid),
            ContextFrontierId::from_uuid(result_frontier_uuid),
            producing_call,
            ContextCompactionRange::inclusive(first, through),
            SemanticTranscriptEntryId::from_uuid(required(&row, "summary_entry_id")?),
        ));
    }

    #[derive(sqlx::FromRow)]
    struct PrecedingNonAcceptedTerminalRow {
        turn_id: Uuid,
        successor_turn_id: Uuid,
        terminal_frontier_id: Uuid,
        direct_selection_id: Uuid,
    }
    let preceding_non_accepted_terminals = sqlx::query_as::<_, PrecedingNonAcceptedTerminalRow>(
        "SELECT terminal.turn_id AS turn_id,
                queued.turn_id AS successor_turn_id,
                turn_lifecycle_effective_terminal_frontier(
                    terminal.session_id, terminal.turn_id
                ) AS terminal_frontier_id,
                effective.direct_selection_id AS direct_selection_id
           FROM queued_input_origin AS queued
           JOIN turn_lifecycle AS terminal
             ON terminal.session_id = $1
            AND terminal.turn_id = CASE queued.priority_kind
                    WHEN 'interrupt_immediately_after'
                        THEN queued.interrupt_predecessor_turn_id
                    WHEN 'ordinary'
                        THEN accepted_input_turn_queue_predecessor(
                            queued.session_id, queued.turn_id
                        )
                END
           JOIN LATERAL turn_origin_effective_model_configuration(
                terminal.turn_id, terminal.session_id
           ) AS effective ON true
          WHERE queued.session_id = $1
            AND goal_turn_is_runtime_relevant(
                    queued.session_id, queued.turn_id
                )
            AND terminal.origin_kind = 'delegation'
            AND (
                queued.priority_kind = 'interrupt_immediately_after'
                OR EXISTS (
                    SELECT 1
                      FROM session_delegation_initial_task AS initial_task
                     WHERE initial_task.child_session_id = terminal.session_id
                       AND initial_task.turn_id = terminal.turn_id
                )
                OR EXISTS (
                    SELECT 1
                      FROM session_delegation_wake_turn_origin AS wake
                     WHERE wake.recipient_session_id = terminal.session_id
                       AND wake.turn_id = terminal.turn_id
                )
            )
            AND (
                terminal.state_kind = 'terminal'
                OR terminal.delegation_runtime_terminal
            )
            AND turn_lifecycle_effective_terminal_frontier(
                    terminal.session_id, terminal.turn_id
                ) IS NOT NULL
          ORDER BY queued.acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    for preceding in &preceding_non_accepted_terminals {
        delegated_turns.insert(turn_id_from_uuid(preceding.turn_id));
        required_frontiers.insert(preceding.terminal_frontier_id);
    }

    let semantic_delegated_turns = sqlx::query_scalar::<_, Uuid>(
        "SELECT DISTINCT subject.turn_id
           FROM (
                SELECT CASE entry.payload_kind
                         WHEN 'model_identity_changed'
                           THEN entry.model_identity_turn_id
                         WHEN 'turn_completed' THEN entry.completed_turn_id
                         WHEN 'turn_failed' THEN entry.failed_turn_id
                         WHEN 'turn_cancelled' THEN entry.cancelled_turn_id
                       END AS turn_id
                  FROM semantic_transcript_entry AS entry
                 WHERE entry.source_session_id = $1
                   AND entry.payload_kind IN (
                        'model_identity_changed',
                        'turn_completed',
                        'turn_failed',
                        'turn_cancelled'
                   )
           ) AS subject
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.session_id = $1
            AND lifecycle.turn_id = subject.turn_id
            AND lifecycle.origin_kind = 'delegation'
            AND (
                lifecycle.state_kind = 'terminal'
                OR lifecycle.delegation_runtime_terminal
            )
          ORDER BY subject.turn_id",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    delegated_turns.extend(semantic_delegated_turns.into_iter().map(turn_id_from_uuid));

    let delegated_turn_ids = delegated_turns
        .iter()
        .map(|turn| turn.into_uuid())
        .collect::<Vec<_>>();
    let delegated_turn_rows = sqlx::query(
        "SELECT lifecycle.turn_id,
                effective.defaults_version,
                effective.direct_selection_id,
                lifecycle.state_kind,
                lifecycle.terminal_disposition_kind,
                lifecycle.delegation_runtime_terminal
           FROM turn_lifecycle AS lifecycle
           JOIN LATERAL turn_origin_effective_model_configuration(
                lifecycle.turn_id, lifecycle.session_id
           ) AS effective ON true
          WHERE lifecycle.session_id = $1
            AND lifecycle.origin_kind = 'delegation'
            AND lifecycle.turn_id = ANY($2)
          ORDER BY lifecycle.turn_id",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&delegated_turn_ids)
    .fetch_all(&mut *connection)
    .await?;
    let mut delegated_turn_facts = Vec::with_capacity(delegated_turn_rows.len());
    for row in delegated_turn_rows {
        let turn = turn_id_from_uuid(required(&row, "turn_id")?);
        let defaults_version =
            defaults_version_from_numeric(required(&row, "defaults_version")?)
                .map_err(|_| SubmitInputCorruption::Inconsistent("delegated defaults version"))?;
        let selected = DirectModelSelection::from_uuid(required(&row, "direct_selection_id")?);
        let state_kind: String = required(&row, "state_kind")?;
        let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let runtime_terminal: bool = required(&row, "delegation_runtime_terminal")?;
        let state = decode_delegated_turn_scheduling_state(
            &state_kind,
            terminal_disposition.as_deref(),
            runtime_terminal,
        )?;
        delegated_turn_facts.push(DelegatedTurnSchedulingFact::new(
            turn,
            defaults_version,
            selected,
            state,
        ));
    }
    if delegated_turn_facts.len() != delegated_turn_ids.len() {
        return Err(SubmitInputCorruption::Missing("delegated turn scheduling fact").into());
    }

    let scheduling_frontier_ids = required_frontiers.iter().copied().collect::<Vec<_>>();
    required_frontiers.extend(
        supplemental_semantic_frontiers
            .iter()
            .map(|frontier| frontier.into_uuid()),
    );
    let required_frontier_ids = required_frontiers.iter().copied().collect::<Vec<_>>();
    let frontier_rows = sqlx::query(
        "WITH RECURSIVE frontier_ids (context_frontier_id) AS (
            SELECT required.context_frontier_id
              FROM UNNEST($2::uuid[]) AS required(context_frontier_id)
            UNION
            SELECT frontier.prefix_context_frontier_id
              FROM frontier_ids
              JOIN context_frontier AS frontier
                ON frontier.owning_session_id = $1
               AND frontier.context_frontier_id =
                       frontier_ids.context_frontier_id
             WHERE frontier.prefix_context_frontier_id IS NOT NULL
        )
        SELECT
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id,
            frontier.member_count,
            delta.member_position,
            delta.source_session_id,
            delta.semantic_entry_id
          FROM frontier_ids
          JOIN context_frontier AS frontier
            ON frontier.owning_session_id = $1
           AND frontier.context_frontier_id =
                   frontier_ids.context_frontier_id
          LEFT JOIN context_frontier_delta AS delta
            ON delta.owning_session_id = frontier.owning_session_id
           AND delta.context_frontier_id =
                   frontier.context_frontier_id
         ORDER BY frontier.context_frontier_id, delta.member_position",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&required_frontier_ids)
    .fetch_all(&mut *connection)
    .await?;
    struct StoredFrontierDelta {
        prefix: Option<Uuid>,
        declared_count: Decimal,
        members: Vec<(Decimal, SessionId, SemanticTranscriptEntryId)>,
    }
    let mut stored_frontiers = BTreeMap::<Uuid, StoredFrontierDelta>::new();
    let mut required_semantic_entries = BTreeSet::new();
    for row in frontier_rows {
        let frontier: Uuid = required(&row, "context_frontier_id")?;
        let prefix: Option<Uuid> = row.try_get("prefix_context_frontier_id")?;
        let declared_count: Decimal = required(&row, "member_count")?;
        let position: Option<Decimal> = row.try_get("member_position")?;
        let source_session: Option<Uuid> = row.try_get("source_session_id")?;
        let semantic_entry: Option<Uuid> = row.try_get("semantic_entry_id")?;
        let stored = stored_frontiers
            .entry(frontier)
            .or_insert_with(|| StoredFrontierDelta {
                prefix,
                declared_count,
                members: Vec::new(),
            });
        if stored.prefix != prefix || stored.declared_count != declared_count {
            return Err(
                SubmitInputCorruption::Inconsistent("context frontier repeated header").into(),
            );
        }
        match (position, source_session, semantic_entry) {
            (Some(position), Some(source_session), Some(semantic_entry)) => {
                required_semantic_entries.insert((source_session, semantic_entry));
                stored.members.push((
                    position,
                    session_id_from_uuid(source_session),
                    SemanticTranscriptEntryId::from_uuid(semantic_entry),
                ));
            }
            (None, None, None) => {}
            _ => {
                return Err(
                    SubmitInputCorruption::Inconsistent("context frontier delta row").into(),
                );
            }
        }
    }

    let semantic_source_sessions = required_semantic_entries
        .iter()
        .map(|(source_session, _)| *source_session)
        .collect::<Vec<_>>();
    let semantic_entry_ids = required_semantic_entries
        .iter()
        .map(|(_, semantic_entry)| *semantic_entry)
        .collect::<Vec<_>>();
    let semantic_rows = sqlx::query(
        "SELECT
            entry.source_session_id,
            entry.semantic_entry_id,
            entry.payload_kind,
            entry.origin_accepted_input_id,
            entry.steering_source_turn_id,
            entry.failed_turn_id,
            entry.cancelled_turn_id,
            entry.assistant_text_value,
            entry.producing_model_call_id,
            entry.assistant_tool_request_id,
            entry.tool_result_request_id,
            entry.tool_result_attempt_id,
            entry.completed_turn_id,
            entry.imported_conversation_id,
            entry.imported_transcript_entry_id,
            entry.assistant_response_part_ordinal,
            entry.model_identity_turn_id,
            entry.model_identity_defaults_version,
            entry.model_identity_direct_selection_id,
            entry.context_summary_value,
            entry.context_summary_producing_call_id,
            entry.context_summary_first_source_session_id,
            entry.context_summary_first_entry_id,
            entry.context_summary_through_source_session_id,
            entry.context_summary_through_entry_id,
            entry.delegated_task_spawning_tool_request_id,
            entry.delegation_message_id,
            entry.delegation_result_awaiting_tool_request_id,
            entry.delegation_result_spawning_tool_request_id,
            delegated_task.task_content AS delegated_task_content,
            task_relation.parent_session_id AS delegated_task_parent_session_id,
            task_relation.parent_turn_id AS delegated_task_parent_turn_id,
            delegated_message.spawning_tool_request_id AS delegation_message_spawning_request_id,
            message_delivery.recipient_session_id AS delegation_message_recipient_session_id,
            message_delivery.delivery_sequence AS delegation_message_delivery_sequence,
            delegated_message.content_text AS delegation_message_content,
            CASE delegated_message.direction
                WHEN 'parent_to_child' THEN message_relation.parent_session_id
                WHEN 'child_to_parent' THEN message_relation.child_session_id
            END AS delegation_message_sender_session_id,
            delegated_wait.child_session_id AS delegation_result_child_session_id,
            delegated_wait.wait_mode AS delegation_result_wait_mode,
            result_delivery.delivery_sequence AS delegation_result_delivery_sequence,
            delegated_result.outcome_kind AS delegation_result_outcome_kind,
            delegated_result.content_text AS delegation_result_content,
            result_event.reason_kind AS delegation_result_reason_kind,
            result_event.provenance_kind AS delegation_result_provenance_kind,
            result_event.provenance_session_id AS delegation_result_provenance_session_id,
            result_event.provenance_turn_id AS delegation_result_provenance_turn_id,
            result_event.provenance_goal_generation AS delegation_result_provenance_goal_generation,
            result_event.provenance_command_id AS delegation_result_provenance_command_id
         FROM semantic_transcript_entry AS entry
         LEFT JOIN session_delegation_initial_task AS delegated_task
           ON delegated_task.spawning_tool_request_id =
                  entry.delegated_task_spawning_tool_request_id
          AND entry.payload_kind = 'delegated_task'
          AND delegated_task.child_session_id = entry.source_session_id
          AND delegated_task.semantic_entry_id = entry.semantic_entry_id
         LEFT JOIN session_delegation AS task_relation
           ON task_relation.spawning_tool_request_id =
                  delegated_task.spawning_tool_request_id
          AND entry.payload_kind = 'delegated_task'
         LEFT JOIN session_message_delivery AS message_delivery
           ON message_delivery.message_id = entry.delegation_message_id
          AND entry.payload_kind = 'delegation_message'
          AND message_delivery.recipient_session_id = entry.source_session_id
         LEFT JOIN session_message AS delegated_message
           ON delegated_message.message_id = message_delivery.message_id
          AND entry.payload_kind = 'delegation_message'
          AND delegated_message.spawning_tool_request_id =
                  message_delivery.spawning_tool_request_id
         LEFT JOIN session_delegation AS message_relation
           ON message_relation.spawning_tool_request_id =
                  delegated_message.spawning_tool_request_id
          AND entry.payload_kind = 'delegation_message'
         LEFT JOIN session_child_result_delivery AS result_delivery
           ON result_delivery.awaiting_tool_request_id =
                  entry.delegation_result_awaiting_tool_request_id
          AND entry.payload_kind = 'delegation_result'
          AND result_delivery.spawning_tool_request_id =
                  entry.delegation_result_spawning_tool_request_id
          AND result_delivery.parent_session_id = entry.source_session_id
         LEFT JOIN session_delegation_wait AS delegated_wait
           ON delegated_wait.awaiting_tool_request_id =
                  result_delivery.awaiting_tool_request_id
          AND entry.payload_kind = 'delegation_result'
          AND delegated_wait.spawning_tool_request_id =
                  result_delivery.spawning_tool_request_id
          AND delegated_wait.parent_session_id = result_delivery.parent_session_id
         LEFT JOIN session_child_result AS delegated_result
           ON delegated_result.spawning_tool_request_id =
                  result_delivery.spawning_tool_request_id
          AND entry.payload_kind = 'delegation_result'
         LEFT JOIN session_delegation_event AS result_event
           ON result_event.spawning_tool_request_id =
                  delegated_result.spawning_tool_request_id
          AND entry.payload_kind = 'delegation_result'
          AND result_event.event_ordinal = delegated_result.event_ordinal
          AND result_event.event_kind = delegated_result.event_kind
        WHERE entry.payload_kind <> 'imported_entry'
          AND (
            entry.source_session_id = $3
            OR (entry.source_session_id, entry.semantic_entry_id) IN (
            SELECT required.source_session_id, required.semantic_entry_id
              FROM UNNEST($1::uuid[], $2::uuid[])
                AS required(source_session_id, semantic_entry_id)
            )
        )
        ORDER BY entry.source_session_id, entry.semantic_entry_id",
    )
    .bind(&semantic_source_sessions)
    .bind(&semantic_entry_ids)
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut semantic_entries = Vec::with_capacity(semantic_rows.len());
    let mut loaded_semantic_entries = imported_session
        .as_ref()
        .into_iter()
        .flat_map(|imported| imported.semantic_entries())
        .map(|entry| {
            (
                entry.source_session().into_uuid(),
                entry.identity().into_uuid(),
            )
        })
        .collect::<BTreeSet<_>>();
    for row in semantic_rows {
        let source_session_uuid: Uuid = required(&row, "source_session_id")?;
        let entry_uuid: Uuid = required(&row, "semantic_entry_id")?;
        if !loaded_semantic_entries.insert((source_session_uuid, entry_uuid)) {
            return Err(SubmitInputCorruption::Inconsistent("duplicate semantic entry").into());
        }
        let source_session = session_id_from_uuid(source_session_uuid);
        let entry = SemanticTranscriptEntryId::from_uuid(entry_uuid);
        let payload_kind: String = required(&row, "payload_kind")?;
        let origin: Option<Uuid> = row.try_get("origin_accepted_input_id")?;
        let steering_source_turn: Option<Uuid> = row.try_get("steering_source_turn_id")?;
        let failed_turn: Option<Uuid> = row.try_get("failed_turn_id")?;
        let cancelled_turn: Option<Uuid> = row.try_get("cancelled_turn_id")?;
        let assistant_text: Option<String> = row.try_get("assistant_text_value")?;
        let producing_call: Option<Uuid> = row.try_get("producing_model_call_id")?;
        let tool_request: Option<Uuid> = row.try_get("assistant_tool_request_id")?;
        let tool_result_request: Option<Uuid> = row.try_get("tool_result_request_id")?;
        let tool_result_attempt: Option<Uuid> = row.try_get("tool_result_attempt_id")?;
        let completed_turn: Option<Uuid> = row.try_get("completed_turn_id")?;
        let imported_conversation: Option<Uuid> = row.try_get("imported_conversation_id")?;
        let imported_transcript_entry: Option<Uuid> =
            row.try_get("imported_transcript_entry_id")?;
        let assistant_response_part_ordinal: Option<Decimal> =
            row.try_get("assistant_response_part_ordinal")?;
        let model_identity_turn: Option<Uuid> = row.try_get("model_identity_turn_id")?;
        let model_identity_defaults_version: Option<Decimal> =
            row.try_get("model_identity_defaults_version")?;
        let model_identity_direct_selection: Option<Uuid> =
            row.try_get("model_identity_direct_selection_id")?;
        let summary_value: Option<String> = row.try_get("context_summary_value")?;
        let summary_call: Option<Uuid> = row.try_get("context_summary_producing_call_id")?;
        let summary_first_session: Option<Uuid> =
            row.try_get("context_summary_first_source_session_id")?;
        let summary_first_entry: Option<Uuid> = row.try_get("context_summary_first_entry_id")?;
        let summary_through_session: Option<Uuid> =
            row.try_get("context_summary_through_source_session_id")?;
        let summary_through_entry: Option<Uuid> =
            row.try_get("context_summary_through_entry_id")?;
        let delegated_task_spawning_request: Option<Uuid> =
            row.try_get("delegated_task_spawning_tool_request_id")?;
        let delegated_task_content: Option<String> = row.try_get("delegated_task_content")?;
        let delegated_task_parent_session: Option<Uuid> =
            row.try_get("delegated_task_parent_session_id")?;
        let delegated_task_parent_turn: Option<Uuid> =
            row.try_get("delegated_task_parent_turn_id")?;
        let delegation_message: Option<Uuid> = row.try_get("delegation_message_id")?;
        let delegation_message_spawning_request: Option<Uuid> =
            row.try_get("delegation_message_spawning_request_id")?;
        let delegation_message_sender: Option<Uuid> =
            row.try_get("delegation_message_sender_session_id")?;
        let delegation_message_recipient: Option<Uuid> =
            row.try_get("delegation_message_recipient_session_id")?;
        let delegation_message_delivery_sequence: Option<Decimal> =
            row.try_get("delegation_message_delivery_sequence")?;
        let delegation_message_content: Option<String> =
            row.try_get("delegation_message_content")?;
        let delegation_result_awaiting_request: Option<Uuid> =
            row.try_get("delegation_result_awaiting_tool_request_id")?;
        let delegation_result_spawning_request: Option<Uuid> =
            row.try_get("delegation_result_spawning_tool_request_id")?;
        let delegation_result_child: Option<Uuid> =
            row.try_get("delegation_result_child_session_id")?;
        let delegation_result_wait_mode: Option<String> =
            row.try_get("delegation_result_wait_mode")?;
        let delegation_result_delivery_sequence: Option<Decimal> =
            row.try_get("delegation_result_delivery_sequence")?;
        let delegation_result_outcome: Option<String> =
            row.try_get("delegation_result_outcome_kind")?;
        let delegation_result_content: Option<String> = row.try_get("delegation_result_content")?;
        let delegation_result_reason: Option<String> =
            row.try_get("delegation_result_reason_kind")?;
        let delegation_result_provenance_kind: Option<String> =
            row.try_get("delegation_result_provenance_kind")?;
        let delegation_result_provenance_session: Option<Uuid> =
            row.try_get("delegation_result_provenance_session_id")?;
        let delegation_result_provenance_turn: Option<Uuid> =
            row.try_get("delegation_result_provenance_turn_id")?;
        let delegation_result_provenance_goal: Option<Decimal> =
            row.try_get("delegation_result_provenance_goal_generation")?;
        let delegation_result_provenance_command: Option<Uuid> =
            row.try_get("delegation_result_provenance_command_id")?;
        let legacy_payload_present = origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || cancelled_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_attempt.is_some()
            || completed_turn.is_some()
            || imported_conversation.is_some()
            || imported_transcript_entry.is_some()
            || assistant_response_part_ordinal.is_some()
            || model_identity_turn.is_some()
            || model_identity_defaults_version.is_some()
            || model_identity_direct_selection.is_some()
            || summary_value.is_some()
            || summary_call.is_some()
            || summary_first_session.is_some()
            || summary_first_entry.is_some()
            || summary_through_session.is_some()
            || summary_through_entry.is_some();
        if payload_kind == "delegated_task" {
            let (Some(spawning_request), Some(parent_session), Some(parent_turn), Some(content)) = (
                delegated_task_spawning_request,
                delegated_task_parent_session,
                delegated_task_parent_turn,
                delegated_task_content,
            ) else {
                return Err(SubmitInputCorruption::Inconsistent("delegated task payload").into());
            };
            if legacy_payload_present
                || tool_result_request.is_some()
                || delegation_message.is_some()
                || delegation_result_awaiting_request.is_some()
                || delegation_result_spawning_request.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("delegated task payload").into());
            }
            let content = delegation_content(content, "delegated_task_content")?;
            semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
                entry,
                source_session,
                InitialSemanticTranscriptEntryPayload::DelegatedTask {
                    spawning_request: ToolRequestId::from_uuid(spawning_request),
                    parent_session: session_id_from_uuid(parent_session),
                    parent_turn: turn_id_from_uuid(parent_turn),
                    content,
                },
            ));
            continue;
        }
        if payload_kind == "delegation_message" {
            let (
                Some(message),
                Some(spawning_request),
                Some(sender),
                Some(recipient),
                Some(delivery_sequence),
                Some(content),
            ) = (
                delegation_message,
                delegation_message_spawning_request,
                delegation_message_sender,
                delegation_message_recipient,
                delegation_message_delivery_sequence,
                delegation_message_content,
            )
            else {
                return Err(
                    SubmitInputCorruption::Inconsistent("delegation message payload").into(),
                );
            };
            if legacy_payload_present
                || tool_result_request.is_some()
                || delegated_task_spawning_request.is_some()
                || delegation_result_awaiting_request.is_some()
                || delegation_result_spawning_request.is_some()
                || recipient != source_session_uuid
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("delegation message payload").into(),
                );
            }
            let delivery_sequence = NonZeroU64::new(
                positive_u64_from_numeric(delivery_sequence)
                    .map_err(|_| SubmitInputCorruption::Inconsistent("delegation delivery"))?,
            )
            .ok_or(SubmitInputCorruption::Inconsistent("delegation delivery"))?;
            let content = delegation_content(content, "delegation_message_content")?;
            semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
                entry,
                source_session,
                InitialSemanticTranscriptEntryPayload::DelegationMessage {
                    spawning_request: ToolRequestId::from_uuid(spawning_request),
                    message: DelegationMessageId::from_uuid(message),
                    sender: session_id_from_uuid(sender),
                    recipient: source_session,
                    delivery_sequence,
                    content,
                },
            ));
            continue;
        }
        if payload_kind == "delegation_result" {
            let (
                Some(awaiting_request),
                Some(spawning_request),
                Some(child),
                Some(wait_mode),
                Some(outcome_kind),
                Some(reason),
                Some(provenance_kind),
                Some(provenance_session),
            ) = (
                delegation_result_awaiting_request,
                delegation_result_spawning_request,
                delegation_result_child,
                delegation_result_wait_mode.as_deref(),
                delegation_result_outcome.as_deref(),
                delegation_result_reason.as_deref(),
                delegation_result_provenance_kind.as_deref(),
                delegation_result_provenance_session,
            )
            else {
                return Err(
                    SubmitInputCorruption::Inconsistent("delegation result payload").into(),
                );
            };
            let mode = decode_delegation_wait_mode(wait_mode)?;
            let delivery_sequence = delegation_result_delivery_sequence
                .map(|value| {
                    positive_u64_from_numeric(value)
                        .ok()
                        .and_then(NonZeroU64::new)
                        .ok_or(SubmitInputCorruption::Inconsistent("delegation delivery"))
                })
                .transpose()?;
            if legacy_payload_present
                || delegated_task_spawning_request.is_some()
                || delegation_message.is_some()
                || (mode == DelegationWaitMode::Foreground
                    && (tool_result_request != Some(awaiting_request)
                        || delivery_sequence.is_some()))
                || (mode == DelegationWaitMode::Background
                    && (tool_result_request.is_some() || delivery_sequence.is_none()))
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("delegation result payload").into(),
                );
            }
            let content = delegation_result_content
                .map(|value| delegation_content(value, "delegation_result_content"))
                .transpose()?;
            let outcome = DelegationOutcome::reconstitute(
                decode_delegation_outcome_kind(outcome_kind)?,
                content,
                decode_delegation_outcome_reason(reason)?,
                decode_delegation_provenance(
                    provenance_kind,
                    provenance_session,
                    delegation_result_provenance_turn,
                    delegation_result_provenance_goal,
                    delegation_result_provenance_command,
                )?,
            )
            .ok_or(SubmitInputCorruption::Inconsistent(
                "delegation result outcome",
            ))?;
            semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
                entry,
                source_session,
                InitialSemanticTranscriptEntryPayload::DelegationResult {
                    awaiting_request: ToolRequestId::from_uuid(awaiting_request),
                    spawning_request: ToolRequestId::from_uuid(spawning_request),
                    child: session_id_from_uuid(child),
                    mode,
                    delivery_sequence,
                    outcome: Box::new(outcome),
                },
            ));
            continue;
        }
        if delegated_task_spawning_request.is_some()
            || delegation_message.is_some()
            || delegation_result_awaiting_request.is_some()
            || delegation_result_spawning_request.is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
        }
        if payload_kind == "context_summary" {
            if origin.is_some()
                || steering_source_turn.is_some()
                || failed_turn.is_some()
                || cancelled_turn.is_some()
                || assistant_text.is_some()
                || producing_call.is_some()
                || tool_request.is_some()
                || tool_result_request.is_some()
                || tool_result_attempt.is_some()
                || completed_turn.is_some()
                || imported_conversation.is_some()
                || imported_transcript_entry.is_some()
                || assistant_response_part_ordinal.is_some()
                || model_identity_turn.is_some()
                || model_identity_defaults_version.is_some()
                || model_identity_direct_selection.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
            }
            let (
                Some(value),
                Some(call),
                Some(first_session),
                Some(first_entry),
                Some(through_session),
                Some(through_entry),
            ) = (
                summary_value,
                summary_call,
                summary_first_session,
                summary_first_entry,
                summary_through_session,
                summary_through_entry,
            )
            else {
                return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
            };
            let payload = InitialSemanticTranscriptEntryPayload::ContextSummary {
                producing_call: ModelCallId::from_uuid(call),
                summarized: ContextCompactionRange::inclusive(
                    SemanticTranscriptEntryRef::from_source(
                        session_id_from_uuid(first_session),
                        SemanticTranscriptEntryId::from_uuid(first_entry),
                    ),
                    SemanticTranscriptEntryRef::from_source(
                        session_id_from_uuid(through_session),
                        SemanticTranscriptEntryId::from_uuid(through_entry),
                    ),
                ),
                value: AssistantText::try_new(value).map_err(|error| {
                    SubmitInputCorruption::InvalidContent {
                        field: "context_summary_value",
                        failure: error.failure(),
                    }
                })?,
            };
            semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
                entry,
                source_session,
                payload,
            ));
            continue;
        }
        if summary_value.is_some()
            || summary_call.is_some()
            || summary_first_session.is_some()
            || summary_first_entry.is_some()
            || summary_through_session.is_some()
            || summary_through_entry.is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
        }
        if payload_kind == "model_identity_changed" {
            if origin.is_some()
                || steering_source_turn.is_some()
                || failed_turn.is_some()
                || cancelled_turn.is_some()
                || assistant_text.is_some()
                || producing_call.is_some()
                || tool_request.is_some()
                || tool_result_request.is_some()
                || tool_result_attempt.is_some()
                || completed_turn.is_some()
                || imported_conversation.is_some()
                || imported_transcript_entry.is_some()
                || assistant_response_part_ordinal.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
            }
            let payload = match (
                model_identity_turn,
                model_identity_defaults_version,
                model_identity_direct_selection,
            ) {
                (Some(turn), Some(defaults_version), Some(selected)) => {
                    InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                        turn: turn_id_from_uuid(turn),
                        defaults_version: defaults_version_from_numeric(defaults_version).map_err(
                            |_| {
                                SubmitInputCorruption::Inconsistent(
                                    "model identity defaults version",
                                )
                            },
                        )?,
                        selected: DirectModelSelection::from_uuid(selected),
                    }
                }
                _ => {
                    return Err(
                        SubmitInputCorruption::Inconsistent("semantic entry payload").into(),
                    );
                }
            };
            semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
                entry,
                source_session,
                payload,
            ));
            continue;
        }
        if model_identity_turn.is_some()
            || model_identity_defaults_version.is_some()
            || model_identity_direct_selection.is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
        }
        let payload = match (
            payload_kind.as_str(),
            origin,
            steering_source_turn,
            failed_turn,
            cancelled_turn,
            assistant_text,
            producing_call,
            tool_request,
            tool_result_request,
            tool_result_attempt,
            completed_turn,
        ) {
            (
                "origin_accepted_input",
                Some(origin),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: accepted_input_id_from_uuid(origin),
            },
            (
                "steering_accepted_input",
                Some(accepted_input),
                Some(source_turn),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: accepted_input_id_from_uuid(accepted_input),
                source_turn: turn_id_from_uuid(source_turn),
            },
            ("turn_failed", None, None, Some(turn), None, None, None, None, None, None, None) => {
                InitialSemanticTranscriptEntryPayload::TurnFailed {
                    turn: turn_id_from_uuid(turn),
                }
            }
            (
                "turn_cancelled",
                None,
                None,
                None,
                Some(turn),
                None,
                None,
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: turn_id_from_uuid(turn),
            },
            (
                "assistant_text",
                None,
                None,
                None,
                None,
                Some(text),
                Some(call),
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::AssistantText {
                producing_call: ModelCallId::from_uuid(call),
                value: AssistantText::try_new(text).map_err(|error| {
                    SubmitInputCorruption::InvalidContent {
                        field: "assistant_text_value",
                        failure: error.failure(),
                    }
                })?,
            },
            (
                "provider_compaction",
                None,
                None,
                None,
                None,
                Some(block),
                Some(call),
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call: ModelCallId::from_uuid(call),
                block: ProviderCompactionBlock::try_new(block).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("provider compaction block")
                })?,
            },
            (
                "provider_reasoning",
                None,
                None,
                None,
                None,
                Some(item),
                Some(call),
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::ProviderReasoning {
                producing_call: ModelCallId::from_uuid(call),
                item: ProviderReasoningItem::try_new(item)
                    .map_err(|_| SubmitInputCorruption::Inconsistent("provider reasoning item"))?,
            },
            (
                "assistant_tool_use",
                None,
                None,
                None,
                None,
                None,
                Some(call),
                Some(request),
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: ModelCallId::from_uuid(call),
                request: ToolRequestId::from_uuid(request),
            },
            (
                "tool_execution_result",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(attempt),
                None,
            ) => InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: signalbox_domain::ToolAttemptId::from_uuid(attempt),
            },
            (
                "tool_denied",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(request),
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::ToolDenied {
                request: ToolRequestId::from_uuid(request),
            },
            (
                "tool_closed_by_turn_end",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(request),
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::ToolClosed {
                request: ToolRequestId::from_uuid(request),
            },
            (
                "turn_completed",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(turn),
            ) => InitialSemanticTranscriptEntryPayload::TurnCompleted {
                turn: turn_id_from_uuid(turn),
            },
            (
                "origin_accepted_input"
                | "steering_accepted_input"
                | "turn_failed"
                | "turn_cancelled"
                | "assistant_text"
                | "provider_compaction"
                | "provider_reasoning"
                | "assistant_tool_use"
                | "tool_execution_result"
                | "tool_denied"
                | "tool_closed_by_turn_end"
                | "turn_completed",
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
            ) => {
                return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
            }
            (value, _, _, _, _, _, _, _, _, _, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "semantic entry payload_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
            entry,
            source_session,
            payload,
        ));
    }
    if !required_semantic_entries.is_subset(&loaded_semantic_entries) {
        return Err(SubmitInputCorruption::Missing("context frontier semantic entry").into());
    }

    if required_frontier_ids
        .iter()
        .any(|frontier| !stored_frontiers.contains_key(frontier))
    {
        return Err(SubmitInputCorruption::Missing("scheduling context frontier").into());
    }
    let mut children = BTreeMap::<Uuid, Vec<Uuid>>::new();
    let mut ready = VecDeque::new();
    for (frontier, stored) in &stored_frontiers {
        if let Some(prefix) = stored.prefix {
            if !stored_frontiers.contains_key(&prefix) {
                return Err(SubmitInputCorruption::Missing("context frontier prefix").into());
            }
            children.entry(prefix).or_default().push(*frontier);
        } else {
            ready.push_back(*frontier);
        }
    }
    let mut reconstructed = BTreeMap::<Uuid, ResolvedContextFrontierReconstitutionInput>::new();
    while let Some(frontier) = ready.pop_front() {
        let stored = &stored_frontiers[&frontier];
        let prefix = stored
            .prefix
            .map(|prefix| {
                reconstructed
                    .get(&prefix)
                    .cloned()
                    .ok_or(SubmitInputCorruption::Missing(
                        "reconstructed context frontier prefix",
                    ))
            })
            .transpose()?;
        let prefix_member_count = prefix.as_ref().map_or(0, |prefix| prefix.entry_count());
        let actual_count = prefix_member_count
            .checked_add(stored.members.len())
            .ok_or(SubmitInputCorruption::Inconsistent(
                "context frontier declared membership",
            ))?;
        let actual_count = u64::try_from(actual_count).map_err(|_| {
            SubmitInputCorruption::Inconsistent("context frontier declared membership")
        })?;
        if stored.declared_count != Decimal::from(actual_count) {
            return Err(SubmitInputCorruption::Inconsistent(
                "context frontier declared membership",
            )
            .into());
        }
        let mut members = Vec::with_capacity(stored.members.len());
        for (index, (position, source_session, semantic_entry)) in stored.members.iter().enumerate()
        {
            let expected_position =
                u64::try_from(prefix_member_count + index + 1).map_err(|_| {
                    SubmitInputCorruption::Inconsistent("context frontier contiguous membership")
                })?;
            if *position != Decimal::from(expected_position) {
                return Err(SubmitInputCorruption::Inconsistent(
                    "context frontier contiguous membership",
                )
                .into());
            }
            members.push(SemanticTranscriptEntryRef::from_source(
                *source_session,
                *semantic_entry,
            ));
        }
        let input = match prefix {
            None => ResolvedContextFrontierReconstitutionInput::new(
                session_id,
                ContextFrontierId::from_uuid(frontier),
                members,
            ),
            Some(prefix) => {
                prefix.derive_appending(ContextFrontierId::from_uuid(frontier), members)
            }
        };
        reconstructed.insert(frontier, input);
        if let Some(successors) = children.get(&frontier) {
            ready.extend(successors.iter().copied());
        }
    }
    if reconstructed.len() != stored_frontiers.len() {
        return Err(SubmitInputCorruption::Inconsistent("context frontier prefix cycle").into());
    }
    let snapshots = scheduling_frontier_ids
        .iter()
        .map(|frontier| {
            reconstructed
                .get(frontier)
                .cloned()
                .ok_or(SubmitInputCorruption::Missing(
                    "reconstructed scheduling context frontier",
                ))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut input = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        turns,
        semantic_entries,
        snapshots,
        active_acceptance_tail,
    );
    if let Some(imported_session) = imported_session {
        input = input.with_imported_session(imported_session);
    }
    for preceding in preceding_non_accepted_terminals {
        input = input.with_preceding_non_accepted_terminal(
            session_id,
            TurnId::from_uuid(preceding.turn_id),
            TurnId::from_uuid(preceding.successor_turn_id),
            ContextFrontierId::from_uuid(preceding.terminal_frontier_id),
            DirectModelSelection::from_uuid(preceding.direct_selection_id),
        );
    }
    input
        .with_model_call_facts(pinned_targets, model_calls)
        .with_context_compaction_facts(compaction_calls, compactions)
        .with_consumed_steering_facts(consumed_steering)
        .with_delegated_consumed_steering_facts(delegated_consumed_steering)
        .with_delegated_turn_facts(delegated_turn_facts)
        .with_steering_continuation_rounds(steering_continuation_rounds)
        .with_continuation_rounds(continuation_rounds)
        .reconstitute()
        .map_err(|error| {
            let (_, failure) = error.into_parts();
            SubmitInputCorruption::Scheduling(failure).into()
        })
}
