use super::load::{load_attachment_blob_facts, load_live_turn_calls, load_origin_contents};
use super::prepared::map_tool_evidence_error;
use super::{ModelCallCorruption, ModelCallRepositoryError, map_scheduling_error, required};
use crate::mapping::{
    accepted_input_id_from_uuid, durable_command_id_from_uuid, input_position_from_numeric,
    positive_u64_from_numeric, session_id_from_uuid, session_id_to_uuid, turn_id_to_uuid,
};
use crate::session::{SessionRepositoryError, load_session_from_connection};
use crate::submit_input::{
    decode_goal_origin_configuration, load_scheduling_projection,
    require_applied_interrupt_from_attempt, require_recorded_batch,
};
use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputLifecycle, ActiveTurnSchedulingReconstitutionInput,
    ConsumedSteeringReconstitutionInput, DelegatedModelCallRecoveryReconstitutionInput,
    DelegatedTurnActivationInput, DelegatedWakeTurnActivationInput, DelegationContent,
    FrozenModelSelection, ModelCallDisposition, ModelCallExecution,
    ModelCallExecutionReconstitutionInput, ModelCallId, ModelCallReconstitutionState,
    ModelTargetCatalog, ModelTargetDefinition, PendingSteeringInput,
    PreparedDelegatedTurnActivation, PreparedToolResultProjection,
    ResolvedContextFrontierReconstitutionInput, ResolvedContextFrontierSnapshot,
    SemanticTranscriptEntry, SemanticTranscriptEntryId, SemanticTranscriptEntryPayload,
    SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef, SessionId,
    ToolApprovalResolution, ToolResultAttemptCorrelation, TurnAttemptId, TurnId,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};
use std::num::NonZeroU64;

pub(crate) async fn lock_session(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), ModelCallRepositoryError> {
    let (session_exists, scheduler): (bool, Option<Uuid>) =
        sqlx::query_as(crate::lock_inventory::START_ELIGIBLE_TURN)
            .bind(session_id_to_uuid(session))
            .fetch_one(connection)
            .await?;
    match (session_exists, scheduler) {
        (true, Some(_)) => Ok(()),
        (true, None) => Err(ModelCallCorruption::Missing("session scheduler row").into()),
        (false, None) => Err(ModelCallRepositoryError::NoLiveExecution),
        (false, Some(_)) => Err(ModelCallCorruption::Inconsistent("orphan scheduler row").into()),
    }
}

pub(super) async fn require_live_execution(
    connection: &mut PgConnection,
    requested_session: SessionId,
    targets: &ModelTargetCatalog,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    // Boxed because a debug `async fn` frame carries every future it awaits
    // inline: this one-line wrapper otherwise puts the whole reconstitution
    // state machine on the caller stack.
    Box::pin(require_live_execution_with_targets(
        connection,
        requested_session,
        Some(targets),
        None,
        None,
    ))
    .await
}

pub(crate) async fn require_live_execution_for_restart(
    connection: &mut PgConnection,
    requested_session: SessionId,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    // Boxed because a debug `async fn` frame carries every future it awaits
    // inline: this one-line wrapper otherwise puts the whole reconstitution
    // state machine on the caller stack.
    Box::pin(require_live_execution_with_targets(
        connection,
        requested_session,
        None,
        None,
        None,
    ))
    .await
}

struct LoadedDelegatedLiveTurn {
    active: signalbox_domain::ActivatedTurn,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    recovery: Option<DelegatedModelCallRecovery>,
}

pub(crate) struct DelegatedModelCallRecovery {
    pub(crate) active: signalbox_domain::ActivatedTurn,
    pub(crate) call: signalbox_domain::EndedModelCall,
    pub(crate) attempt: signalbox_domain::EndedTurnAttempt,
    pub(crate) source_snapshot: ResolvedContextFrontierSnapshot,
}

pub(crate) async fn load_delegated_runner_recovery_for_interrupt(
    connection: &mut PgConnection,
    requested_session: SessionId,
) -> Result<
    Option<(
        signalbox_domain::ActivatedTurn,
        ResolvedContextFrontierSnapshot,
    )>,
    ModelCallRepositoryError,
> {
    let has_delegated_runner_recovery = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM turn_lifecycle
             WHERE session_id = $1
               AND origin_kind = 'delegation'
               AND state_kind = 'active'
               AND active_phase_kind = 'awaiting_runner_recovery'
               AND NOT delegation_runtime_terminal
               AND goal_turn_is_runtime_relevant(session_id, turn_id)
        )",
    )
    .bind(session_id_to_uuid(requested_session))
    .fetch_one(&mut *connection)
    .await?;
    if !has_delegated_runner_recovery {
        return Ok(None);
    }

    let session = match load_session_from_connection(connection, requested_session).await {
        Ok(Some(session)) => session,
        Ok(None) => return Ok(None),
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(ModelCallCorruption::CurrentSession(error).into());
        }
    };
    // Boxed for the same reason: the scheduling projection reconstitutes the
    // whole accepted-input order, and awaiting it inline places that frame on
    // top of this one.
    let scheduling = Box::pin(load_scheduling_projection(connection, session))
        .await
        .map_err(map_scheduling_error)?;
    Ok(
        load_delegated_live_turn(connection, requested_session, &scheduling)
            .await?
            .filter(|loaded| {
                matches!(
                    loaded.active.phase(),
                    signalbox_domain::ActiveTurnPhase::AwaitingRunnerRecovery { .. }
                )
            })
            .map(|loaded| (loaded.active, loaded.starting_snapshot)),
    )
}

pub(crate) async fn load_delegated_model_call_recovery(
    connection: &mut PgConnection,
    session: SessionId,
    scheduling: &signalbox_domain::AcceptedInputSchedulingProjection,
) -> Result<Option<DelegatedModelCallRecovery>, ModelCallRepositoryError> {
    Ok(load_delegated_live_turn(connection, session, scheduling)
        .await?
        .and_then(|loaded| loaded.recovery))
}

pub(super) async fn require_live_execution_with_targets(
    connection: &mut PgConnection,
    requested_session: SessionId,
    configured_targets: Option<&ModelTargetCatalog>,
    continuation_snapshot: Option<ResolvedContextFrontierReconstitutionInput>,
    uncommitted_tool_result_projection: Option<PreparedToolResultProjection>,
) -> Result<ModelCallExecution, ModelCallRepositoryError> {
    let session = match load_session_from_connection(connection, requested_session).await {
        Ok(Some(session)) => session,
        Ok(None) => return Err(ModelCallRepositoryError::NoLiveExecution),
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(ModelCallCorruption::CurrentSession(error).into());
        }
    };
    // Boxed for the same reason: the scheduling projection reconstitutes the
    // whole accepted-input order, and awaiting it inline places that frame on
    // top of this one.
    let scheduling = Box::pin(load_scheduling_projection(connection, session))
        .await
        .map_err(map_scheduling_error)?;
    let delegated = load_delegated_live_turn(connection, requested_session, &scheduling).await?;
    let (active_turn, starting_snapshot) = match delegated {
        Some(loaded) => (loaded.active, loaded.starting_snapshot),
        None => {
            let active = scheduling
                .active_turn_execution()
                .ok_or(ModelCallRepositoryError::NoLiveExecution)?;
            let starting = scheduling
                .resolved_snapshot(active.start().frontier().snapshot())
                .cloned()
                .ok_or(ModelCallCorruption::Missing("starting snapshot"))?;
            (active.into(), starting)
        }
    };
    if !matches!(
        active_turn.phase(),
        signalbox_domain::ActiveTurnPhase::Running { .. }
    ) {
        return Err(ModelCallRepositoryError::NoLiveExecution);
    }
    let (pinned_target, calls) =
        load_live_turn_calls(connection, requested_session, active_turn.turn()).await?;
    let signalbox_domain::ActiveTurnPhase::Running { current_attempt } = active_turn.phase() else {
        return Err(ModelCallRepositoryError::NoLiveExecution);
    };
    let availability_successor: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM credential_pool_availability_successor
              WHERE successor_turn_attempt_id = $1
         )",
    )
    .bind(current_attempt.id().into_uuid())
    .fetch_one(&mut *connection)
    .await?;
    let call_snapshot = match calls
        .first()
        .filter(|call| call.frontier() != starting_snapshot.frontier().snapshot())
    {
        Some(call) => {
            Some(load_call_snapshot(connection, requested_session, call.frontier()).await?)
        }
        None => None,
    };
    let successor_snapshot = if availability_successor && call_snapshot.is_none() {
        load_availability_predecessor_snapshot(
            connection,
            requested_session,
            current_attempt.id(),
            starting_snapshot.frontier().snapshot(),
        )
        .await?
    } else {
        None
    };
    let current_snapshot = call_snapshot
        .as_ref()
        .or(continuation_snapshot.as_ref())
        .or(successor_snapshot.as_ref());
    let frontier_references = current_snapshot.as_ref().map_or_else(
        || starting_snapshot.ordered_entries().collect::<Vec<_>>(),
        |snapshot| snapshot.ordered_entries().to_vec(),
    );
    let frontier_entries = frontier_references
        .iter()
        .map(|reference| {
            scheduling
                .semantic_entry(*reference)
                .cloned()
                .ok_or(ModelCallCorruption::Missing("frontier semantic entry"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let origin_contents = load_origin_contents(
        connection,
        &frontier_entries,
        active_turn.pending_steering(),
        active_turn.consumed_steering(),
    )
    .await?;
    let attachment_blob_facts = load_attachment_blob_facts(connection, &origin_contents).await?;
    let tool_result_correlations =
        load_tool_result_correlations(connection, &frontier_entries).await?;
    let tool_inadmissible_correlations =
        load_tool_inadmissible_correlations(connection, &frontier_entries).await?;
    let tool_denial_correlations =
        load_tool_denial_correlations(connection, &frontier_entries).await?;
    let recovered_targets;
    let targets = if let Some(targets) = configured_targets {
        targets.clone()
    } else {
        let mut definitions = calls
            .iter()
            .map(|call| {
                let direct = match call.selection() {
                    FrozenModelSelection::Direct(direct) => direct,
                    FrozenModelSelection::FrozenAlias { definition, .. } => definition.selected(),
                };
                ModelTargetDefinition::new(direct, call.target())
            })
            .collect::<Vec<_>>();
        if let Some(pinned) = pinned_target
            && calls.is_empty()
        {
            let direct = match *active_turn.configuration().effective().model() {
                FrozenModelSelection::Direct(direct) => direct,
                FrozenModelSelection::FrozenAlias { definition, .. } => definition.selected(),
            };
            definitions.push(ModelTargetDefinition::new(direct, pinned.target()));
        }
        recovered_targets = ModelTargetCatalog::try_from_definitions(definitions)
            .map_err(|_| ModelCallCorruption::Inconsistent("recovery model-target catalog"))?;
        recovered_targets
    };

    let mut input = ModelCallExecutionReconstitutionInput::new(
        active_turn,
        targets,
        starting_snapshot,
        frontier_entries,
        origin_contents,
        pinned_target,
        calls,
    )
    .with_attachment_blob_facts(attachment_blob_facts)
    .with_tool_result_correlations(tool_result_correlations)
    .with_tool_denial_correlations(tool_denial_correlations)
    .with_tool_inadmissible_correlations(tool_inadmissible_correlations);
    if availability_successor {
        input = input.with_availability_successor();
    }
    if let Some(projection) = uncommitted_tool_result_projection {
        input = input.with_uncommitted_tool_result_projection(projection);
    }
    if let Some(call_snapshot) = call_snapshot {
        input = input.with_call_snapshot(call_snapshot);
    } else if let Some(snapshot) = continuation_snapshot.or(successor_snapshot) {
        input = input.with_continuation_snapshot(snapshot);
    }
    input.reconstitute().map_err(|error| {
        let (_, failure) = error.into_parts();
        ModelCallCorruption::Execution(failure).into()
    })
}

async fn load_delegated_live_turn(
    connection: &mut PgConnection,
    session: SessionId,
    scheduling: &signalbox_domain::AcceptedInputSchedulingProjection,
) -> Result<Option<LoadedDelegatedLiveTurn>, ModelCallRepositoryError> {
    let row = sqlx::query(
        "SELECT
            task.spawning_tool_request_id,
            task.turn_id,
            task.semantic_entry_id,
            task.task_content,
            relation.parent_session_id,
            relation.parent_turn_id,
            lifecycle.starting_frontier_id,
            attempt.turn_attempt_id AS projection_attempt_id,
            attempt.state_kind AS attempt_state_kind,
            attempt.end_variant,
            attempt.end_disposition,
            attempt.interrupt_command_id,
            attempt.interrupt_predecessor_turn_id AS attempt_interrupt_predecessor_turn_id,
            lifecycle.active_phase_kind,
            lifecycle.recovery_model_call_id,
            lifecycle.pinned_provider_model_identity_id,
            lifecycle.runner_recovery_runner_id,
            lifecycle.runner_recovery_placement_revision,
            lifecycle.runner_recovery_tool_attempt_id,
            defaults.session_id AS goal_defaults_session_id,
            task.defaults_version AS queued_defaults_version,
            defaults.version AS goal_defaults_version,
            defaults.model_selection_kind AS goal_defaults_model_kind,
            defaults.direct_model_selection_id AS goal_defaults_direct_id,
            defaults.model_alias_id AS goal_defaults_alias_id,
            defaults.dangerous_tool_auto_approval AS goal_defaults_tool_auto_approval,
            defaults.model_settings AS goal_defaults_model_settings,
            task.requested_model_kind,
            task.requested_direct_model_selection_id,
            task.requested_model_alias_id,
            task.frozen_model_kind,
            task.frozen_direct_model_selection_id,
            task.frozen_model_alias_id,
            task.frozen_alias_selected_direct_id
         FROM turn_lifecycle AS lifecycle
         JOIN session_delegation_initial_task AS task
           ON task.turn_id = lifecycle.turn_id
          AND task.child_session_id = lifecycle.session_id
          AND task.admission_position = lifecycle.acceptance_position
         JOIN session_delegation AS relation
           ON relation.spawning_tool_request_id = task.spawning_tool_request_id
          AND relation.child_session_id = task.child_session_id
         JOIN turn_attempt AS attempt
           ON attempt.turn_id = lifecycle.turn_id
          AND attempt.session_id = lifecycle.session_id
          AND (
                (
                    lifecycle.active_phase_kind = 'running'
                    AND attempt.turn_attempt_id = lifecycle.current_attempt_id
                )
                OR (
                    lifecycle.active_phase_kind = 'awaiting_runner_recovery'
                    AND attempt.state_kind = 'ended'
                    AND attempt.end_variant = 'without_stop'
                    AND attempt.end_disposition = 'yielded_to_durable_wait'
                    AND attempt.interrupt_command_id IS NULL
                    AND attempt.interrupt_predecessor_turn_id IS NULL
                    AND NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt AS continuation
                         WHERE continuation.continued_from_attempt_id =
                                attempt.turn_attempt_id
                    )
                )
                OR (
                    lifecycle.active_phase_kind = 'awaiting_model_call_recovery'
                    AND attempt.turn_attempt_id = lifecycle.current_attempt_id
                    AND attempt.state_kind = 'ended'
                    AND attempt.end_variant IN ('without_stop', 'after_cancellation')
                    AND attempt.end_disposition IN ('ambiguous', 'lost')
                )
          )
         JOIN session_defaults_version AS defaults
           ON defaults.session_id = task.child_session_id
          AND defaults.version = task.defaults_version
        WHERE lifecycle.session_id = $1
          AND lifecycle.origin_kind = 'delegation'
          AND lifecycle.state_kind = 'active'
          AND NOT lifecycle.delegation_runtime_terminal
          AND lifecycle.active_phase_kind IN (
                'running', 'awaiting_runner_recovery',
                'awaiting_model_call_recovery'
          )
          AND goal_turn_is_runtime_relevant(
                lifecycle.session_id, lifecycle.turn_id
          )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return load_delegated_live_wake_turn(connection, session, scheduling).await;
    };
    let turn = TurnId::from_uuid(required(&row, "turn_id")?);
    let spawning_request =
        signalbox_domain::ToolRequestId::from_uuid(required(&row, "spawning_tool_request_id")?);
    let task = DelegationContent::try_new(required(&row, "task_content")?)
        .map_err(|_| ModelCallCorruption::Inconsistent("delegated task content"))?;
    let configuration =
        decode_goal_origin_configuration(&row, session).map_err(map_scheduling_error)?;
    let starting_frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "starting_frontier_id")?);
    let initial_attempt =
        signalbox_domain::TurnAttemptId::from_uuid(required(&row, "projection_attempt_id")?);
    let task_entry = SemanticTranscriptEntryReconstitutionInput::new(
        SemanticTranscriptEntryId::from_uuid(required(&row, "semantic_entry_id")?),
        session,
        SemanticTranscriptEntryPayload::DelegatedTask {
            spawning_request,
            parent_session: SessionId::from_uuid(required(&row, "parent_session_id")?),
            parent_turn: TurnId::from_uuid(required(&row, "parent_turn_id")?),
            content: task.clone(),
        },
    );
    let prepared = PreparedDelegatedTurnActivation::prepare(DelegatedTurnActivationInput {
        session,
        turn,
        spawning_request,
        task,
        task_entry,
        configuration,
        starting_frontier,
        initial_attempt,
    })
    .ok_or(ModelCallCorruption::Inconsistent(
        "delegated live-turn projection",
    ))?;
    let recovery_call = (required::<String>(&row, "active_phase_kind")?
        == "awaiting_model_call_recovery")
        .then(|| required::<Uuid>(&row, "recovery_model_call_id").map(ModelCallId::from_uuid))
        .transpose()?;
    let phase = decode_delegated_active_phase(connection, &row, turn, initial_attempt).await?;
    finish_loaded_delegated_turn(
        connection,
        session,
        turn,
        starting_frontier,
        prepared,
        phase,
        recovery_call,
    )
    .await
    .map(Some)
}

async fn finish_loaded_delegated_turn(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    starting_frontier: signalbox_domain::ContextFrontierId,
    prepared: PreparedDelegatedTurnActivation,
    phase: ActiveTurnSchedulingReconstitutionInput,
    recovery_call: Option<ModelCallId>,
) -> Result<LoadedDelegatedLiveTurn, ModelCallRepositoryError> {
    let pending = load_delegated_pending_steering(connection, session, turn).await?;
    let consumed = load_delegated_consumed_steering(connection, session, turn).await?;
    let stored_snapshot = load_call_snapshot(connection, session, starting_frontier)
        .await?
        .reconstitute()
        .ok_or(ModelCallCorruption::Inconsistent(
            "delegated starting snapshot",
        ))?;

    if let Some(recovery_call_id) = recovery_call {
        let (pinned, calls) = load_live_turn_calls(connection, session, turn).await?;
        let pinned = pinned.ok_or(ModelCallCorruption::Missing(
            "delegated recovery pinned target",
        ))?;
        let recovery_call = calls
            .into_iter()
            .find(|call| {
                call.id() == recovery_call_id
                    && call.state()
                        == ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous)
            })
            .ok_or(ModelCallCorruption::Missing(
                "delegated recovery model call",
            ))?;
        let source_snapshot =
            load_call_snapshot(connection, session, recovery_call.frontier()).await?;
        let (active, call, attempt, source_snapshot, prepared_snapshot) = prepared
            .with_reconstituted_model_call_recovery(
                DelegatedModelCallRecoveryReconstitutionInput::new(
                    phase,
                    pinned,
                    recovery_call,
                    source_snapshot,
                    pending,
                    consumed,
                ),
            )
            .ok_or(ModelCallCorruption::Inconsistent(
                "delegated recovery evidence",
            ))?;
        if stored_snapshot != prepared_snapshot {
            return Err(ModelCallCorruption::Inconsistent("delegated starting snapshot").into());
        }
        return Ok(LoadedDelegatedLiveTurn {
            active: active.clone(),
            starting_snapshot: stored_snapshot,
            recovery: Some(DelegatedModelCallRecovery {
                active,
                call,
                attempt,
                source_snapshot,
            }),
        });
    }

    let (active, _, prepared_snapshot) =
        prepared
            .with_reconstituted_phase(phase)
            .ok_or(ModelCallCorruption::Inconsistent(
                "delegated live-turn phase",
            ))?;
    let active = active
        .with_pending_steering(pending)
        .ok_or(ModelCallCorruption::Inconsistent(
            "delegated pending steering",
        ))?
        .with_consumed_steering(consumed)
        .ok_or(ModelCallCorruption::Inconsistent(
            "delegated consumed steering",
        ))?;
    if stored_snapshot != prepared_snapshot {
        return Err(ModelCallCorruption::Inconsistent("delegated starting snapshot").into());
    }
    Ok(LoadedDelegatedLiveTurn {
        active: active.into(),
        starting_snapshot: stored_snapshot,
        recovery: None,
    })
}

async fn decode_delegated_active_phase(
    connection: &mut PgConnection,
    row: &PgRow,
    turn: TurnId,
    projection_attempt: signalbox_domain::TurnAttemptId,
) -> Result<ActiveTurnSchedulingReconstitutionInput, ModelCallRepositoryError> {
    match required::<String>(row, "active_phase_kind")?.as_str() {
        "running" => match required::<String>(row, "attempt_state_kind")?.as_str() {
            "prepared" => Ok(ActiveTurnSchedulingReconstitutionInput::prepared(
                turn,
                projection_attempt,
            )),
            "running" => Ok(ActiveTurnSchedulingReconstitutionInput::running(
                turn,
                projection_attempt,
            )),
            value => Err(ModelCallCorruption::Unsupported {
                field: "delegated turn attempt state",
                value: value.to_owned(),
            }
            .into()),
        },
        "awaiting_runner_recovery" => {
            let runner =
                signalbox_domain::RunnerId::from_uuid(required(row, "runner_recovery_runner_id")?);
            let revision =
                positive_u64_from_numeric(required(row, "runner_recovery_placement_revision")?)
                    .ok()
                    .and_then(signalbox_domain::RunnerGeneration::try_from_u64)
                    .ok_or(ModelCallCorruption::Inconsistent(
                        "delegated runner recovery placement revision",
                    ))?;
            Ok(
                ActiveTurnSchedulingReconstitutionInput::awaiting_runner_recovery(
                    turn,
                    runner,
                    revision,
                    row.try_get::<Option<Uuid>, _>("runner_recovery_tool_attempt_id")?
                        .map(signalbox_domain::ToolAttemptId::from_uuid),
                    None,
                ),
            )
        }
        "awaiting_model_call_recovery" => {
            let call = ModelCallId::from_uuid(required(row, "recovery_model_call_id")?);
            if required::<String>(row, "attempt_state_kind")? != "ended" {
                return Err(
                    ModelCallCorruption::Inconsistent("delegated recovery attempt state").into(),
                );
            }
            match (
                required::<String>(row, "end_variant")?.as_str(),
                required::<String>(row, "end_disposition")?.as_str(),
            ) {
                ("without_stop", "ambiguous") => Ok(
                    ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery(
                        turn,
                        projection_attempt,
                        call,
                    ),
                ),
                ("without_stop", "lost") => Ok(
                    ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_restart(
                        turn,
                        projection_attempt,
                        call,
                    ),
                ),
                ("after_cancellation", disposition @ ("ambiguous" | "lost")) => {
                    let command = durable_command_id_from_uuid(required(
                        row,
                        "interrupt_command_id",
                    )?)
                    .map_err(|_| {
                        ModelCallCorruption::Inconsistent(
                            "delegated recovery interrupt identity",
                        )
                    })?;
                    let recorded = require_recorded_batch(connection, &[command])
                        .await
                        .map_err(map_scheduling_error)?;
                    let interrupt = require_applied_interrupt_from_attempt(row, turn, &recorded)
                        .map_err(map_scheduling_error)?;
                    if disposition == "ambiguous" {
                        Ok(ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_cancellation(
                            turn,
                            projection_attempt,
                            call,
                            interrupt,
                        ))
                    } else {
                        Ok(ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_cancellation_restart(
                            turn,
                            projection_attempt,
                            call,
                            interrupt,
                        ))
                    }
                }
                _ => Err(ModelCallCorruption::Inconsistent(
                    "delegated recovery attempt end",
                )
                .into()),
            }
        }
        value => Err(ModelCallCorruption::Unsupported {
            field: "delegated active phase",
            value: value.to_owned(),
        }
        .into()),
    }
}

async fn load_delegated_live_wake_turn(
    connection: &mut PgConnection,
    session: SessionId,
    scheduling: &signalbox_domain::AcceptedInputSchedulingProjection,
) -> Result<Option<LoadedDelegatedLiveTurn>, ModelCallRepositoryError> {
    let row = sqlx::query(
        "SELECT
            wake.turn_id,
            wake.first_delivery_sequence,
            wake.through_delivery_sequence,
            predecessor.turn_id AS predecessor_turn_id,
            turn_lifecycle_effective_terminal_frontier(
                predecessor.session_id, predecessor.turn_id
            ) AS predecessor_frontier_id,
            lifecycle.starting_frontier_id,
            attempt.turn_attempt_id AS projection_attempt_id,
            attempt.state_kind AS attempt_state_kind,
            attempt.end_variant,
            attempt.end_disposition,
            attempt.interrupt_command_id,
            attempt.interrupt_predecessor_turn_id AS attempt_interrupt_predecessor_turn_id,
            lifecycle.active_phase_kind,
            lifecycle.recovery_model_call_id,
            lifecycle.pinned_provider_model_identity_id,
            lifecycle.runner_recovery_runner_id,
            lifecycle.runner_recovery_placement_revision,
            lifecycle.runner_recovery_tool_attempt_id,
            defaults.session_id AS goal_defaults_session_id,
            wake.defaults_version AS queued_defaults_version,
            defaults.version AS goal_defaults_version,
            defaults.model_selection_kind AS goal_defaults_model_kind,
            defaults.direct_model_selection_id AS goal_defaults_direct_id,
            defaults.model_alias_id AS goal_defaults_alias_id,
            defaults.dangerous_tool_auto_approval AS goal_defaults_tool_auto_approval,
            defaults.model_settings AS goal_defaults_model_settings,
            wake.requested_model_kind,
            wake.requested_direct_model_selection_id,
            wake.requested_model_alias_id,
            wake.frozen_model_kind,
            wake.frozen_direct_model_selection_id,
            wake.frozen_model_alias_id,
            wake.frozen_alias_selected_direct_id
         FROM session_delegation_wake_turn_origin AS wake
         JOIN turn_lifecycle AS lifecycle
           ON lifecycle.turn_id = wake.turn_id
          AND lifecycle.session_id = wake.recipient_session_id
          AND lifecycle.acceptance_position = wake.admission_position
          AND lifecycle.origin_kind = 'delegation'
          AND lifecycle.state_kind = 'active'
          AND NOT lifecycle.delegation_runtime_terminal
          AND lifecycle.active_phase_kind IN (
                'running', 'awaiting_runner_recovery',
                'awaiting_model_call_recovery'
          )
         JOIN turn_lifecycle AS predecessor
           ON predecessor.turn_id = lifecycle.immediate_predecessor_turn_id
          AND predecessor.session_id = lifecycle.session_id
          AND (
                predecessor.delegation_runtime_terminal
                OR (
                    predecessor.state_kind = 'terminal'
                    AND predecessor.terminal_disposition_kind IN (
                        'failed', 'completed', 'refused', 'cancelled',
                        'reconciliation_required'
                    )
                )
          )
         JOIN turn_attempt AS attempt
           ON attempt.turn_id = lifecycle.turn_id
          AND attempt.session_id = lifecycle.session_id
          AND (
                (
                    lifecycle.active_phase_kind = 'running'
                    AND attempt.turn_attempt_id = lifecycle.current_attempt_id
                )
                OR (
                    lifecycle.active_phase_kind = 'awaiting_runner_recovery'
                    AND attempt.state_kind = 'ended'
                    AND attempt.end_variant = 'without_stop'
                    AND attempt.end_disposition = 'yielded_to_durable_wait'
                    AND attempt.interrupt_command_id IS NULL
                    AND attempt.interrupt_predecessor_turn_id IS NULL
                    AND NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt AS continuation
                         WHERE continuation.continued_from_attempt_id =
                                attempt.turn_attempt_id
                    )
                )
                OR (
                    lifecycle.active_phase_kind = 'awaiting_model_call_recovery'
                    AND attempt.turn_attempt_id = lifecycle.current_attempt_id
                    AND attempt.state_kind = 'ended'
                    AND attempt.end_variant IN ('without_stop', 'after_cancellation')
                    AND attempt.end_disposition IN ('ambiguous', 'lost')
                )
          )
         JOIN session_defaults_version AS defaults
           ON defaults.session_id = wake.recipient_session_id
          AND defaults.version = wake.defaults_version
        WHERE wake.recipient_session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let first_numeric: Decimal = required(&row, "first_delivery_sequence")?;
    let through_numeric: Decimal = required(&row, "through_delivery_sequence")?;
    let first = NonZeroU64::new(
        positive_u64_from_numeric(first_numeric)
            .map_err(|_| ModelCallCorruption::Inconsistent("wake delivery range"))?,
    )
    .ok_or(ModelCallCorruption::Inconsistent("wake delivery range"))?;
    let through = NonZeroU64::new(
        positive_u64_from_numeric(through_numeric)
            .map_err(|_| ModelCallCorruption::Inconsistent("wake delivery range"))?,
    )
    .ok_or(ModelCallCorruption::Inconsistent("wake delivery range"))?;
    let delivery_rows = sqlx::query_as::<_, (Decimal, Uuid)>(
        "SELECT pending.delivery_sequence,
                delegation_delivery_semantic_entry(
                    pending.recipient_session_id, pending.delivery_sequence
                )
           FROM session_pending_delivery AS pending
          WHERE pending.recipient_session_id = $1
            AND pending.delivery_sequence BETWEEN $2 AND $3
          ORDER BY pending.delivery_sequence",
    )
    .bind(session_id_to_uuid(session))
    .bind(first_numeric)
    .bind(through_numeric)
    .fetch_all(&mut *connection)
    .await?;
    let mut deliveries = Vec::with_capacity(delivery_rows.len());
    for (_, entry) in delivery_rows {
        let reference = SemanticTranscriptEntryRef::from_source(
            session,
            SemanticTranscriptEntryId::from_uuid(entry),
        );
        let semantic = scheduling
            .semantic_entry(reference)
            .ok_or(ModelCallCorruption::Missing("wake semantic entry"))?;
        deliveries.push(SemanticTranscriptEntryReconstitutionInput::new(
            semantic.identity(),
            semantic.source_session(),
            semantic.payload().clone(),
        ));
    }
    let turn = TurnId::from_uuid(required(&row, "turn_id")?);
    let predecessor = TurnId::from_uuid(required(&row, "predecessor_turn_id")?);
    let predecessor_frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "predecessor_frontier_id")?);
    let predecessor_snapshot = scheduling
        .resolved_snapshot(predecessor_frontier)
        .cloned()
        .ok_or(ModelCallCorruption::Missing("wake predecessor snapshot"))?;
    let configuration =
        decode_goal_origin_configuration(&row, session).map_err(map_scheduling_error)?;
    let starting_frontier =
        signalbox_domain::ContextFrontierId::from_uuid(required(&row, "starting_frontier_id")?);
    let initial_attempt =
        signalbox_domain::TurnAttemptId::from_uuid(required(&row, "projection_attempt_id")?);
    let prepared =
        PreparedDelegatedTurnActivation::prepare_wake(DelegatedWakeTurnActivationInput {
            session,
            turn,
            first_delivery_sequence: first,
            through_delivery_sequence: through,
            deliveries,
            predecessor,
            predecessor_snapshot,
            configuration,
            starting_frontier,
            initial_attempt,
        })
        .ok_or(ModelCallCorruption::Inconsistent(
            "delegated wake live-turn projection",
        ))?;
    let recovery_call = (required::<String>(&row, "active_phase_kind")?
        == "awaiting_model_call_recovery")
        .then(|| required::<Uuid>(&row, "recovery_model_call_id").map(ModelCallId::from_uuid))
        .transpose()?;
    let phase = decode_delegated_active_phase(connection, &row, turn, initial_attempt).await?;
    finish_loaded_delegated_turn(
        connection,
        session,
        turn,
        starting_frontier,
        prepared,
        phase,
        recovery_call,
    )
    .await
    .map(Some)
}

async fn load_delegated_pending_steering(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Vec<PendingSteeringInput>, ModelCallRepositoryError> {
    let rows = sqlx::query(
        "SELECT accepted_input_id, acceptance_position
           FROM accepted_input
          WHERE session_id = $1
            AND disposition_kind = 'pending_steering'
            AND expected_active_turn_id = $2
          ORDER BY acceptance_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter()
        .map(|row| {
            let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
            let position = input_position_from_numeric(required(&row, "acceptance_position")?)
                .map_err(|_| ModelCallCorruption::Inconsistent("delegated steering position"))?;
            PendingSteeringInput::reconstitute(
                AcceptedInputLifecycle::new(
                    accepted_input,
                    AcceptedInputDisposition::PendingSteering {
                        binding: signalbox_domain::SteeringBinding::new(turn),
                    },
                ),
                position,
                turn,
            )
            .ok_or_else(|| ModelCallCorruption::Inconsistent("delegated pending steering").into())
        })
        .collect()
}

async fn load_delegated_consumed_steering(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Vec<ConsumedSteeringReconstitutionInput>, ModelCallRepositoryError> {
    let rows = sqlx::query(
        "SELECT accepted_input_id, acceptance_position, consuming_model_call_id
           FROM accepted_input
          WHERE session_id = $1
            AND disposition_kind = 'consumed_as_steering'
            AND expected_active_turn_id = $2
          ORDER BY acceptance_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_all(&mut *connection)
    .await?;
    rows.into_iter()
        .map(|row| {
            let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
            let position = input_position_from_numeric(required(&row, "acceptance_position")?)
                .map_err(|_| ModelCallCorruption::Inconsistent("delegated steering position"))?;
            let call = ModelCallId::from_uuid(required(&row, "consuming_model_call_id")?);
            Ok(ConsumedSteeringReconstitutionInput::new(
                session,
                AcceptedInputLifecycle::new(
                    accepted_input,
                    AcceptedInputDisposition::ConsumedAsSteering { call },
                ),
                position,
                turn,
            ))
        })
        .collect()
}

pub(super) async fn load_tool_inadmissible_correlations(
    connection: &mut PgConnection,
    entries: &[SemanticTranscriptEntry],
) -> Result<Vec<signalbox_domain::ToolRequest>, ModelCallRepositoryError> {
    let requests = entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::ToolInadmissible { request } => Some(*request),
            _ => None,
        })
        .collect::<Vec<_>>();
    Ok(crate::tool_loop::load_requests_by_id(connection, &requests)
        .await
        .map_err(map_tool_evidence_error)?
        .into_values()
        .collect())
}

pub(super) async fn load_tool_denial_correlations(
    connection: &mut PgConnection,
    frontier_entries: &[SemanticTranscriptEntry],
) -> Result<Vec<ToolApprovalResolution>, ModelCallRepositoryError> {
    let requests = frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::ToolDenied { request } => Some(request.into_uuid()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT approval.request_id, approval.decision_kind,
                approval.decision_source, approval.denial_reason,
                approval.user_command_id,
                approval.delegate_model_selection_id,
                approval.delegate_model_call_id, approval.rationale
           FROM tool_approval_decision AS approval
          WHERE approval.request_id = ANY($1)",
    )
    .bind(&requests)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != requests.len() {
        return Err(ModelCallCorruption::Inconsistent("tool-denial resolution ownership").into());
    }
    crate::tool_loop::decode_approvals(connection, rows)
        .await
        .map_err(map_tool_evidence_error)
}

pub(super) async fn load_tool_result_correlations(
    connection: &mut PgConnection,
    frontier_entries: &[SemanticTranscriptEntry],
) -> Result<Vec<ToolResultAttemptCorrelation>, ModelCallRepositoryError> {
    let attempts = frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => {
                Some(attempt.into_uuid())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if attempts.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        "SELECT attempt.attempt_id,
                request.request_id,
                request.producing_model_call_id
           FROM tool_attempt AS attempt
           JOIN tool_request AS request
             ON request.request_id = attempt.request_id
            AND request.session_id = attempt.session_id
            AND request.turn_id = attempt.turn_id
          WHERE attempt.attempt_id = ANY($1)",
    )
    .bind(&attempts)
    .fetch_all(connection)
    .await?;
    if rows.len() != attempts.len() {
        return Err(ModelCallCorruption::Inconsistent("tool-result attempt ownership").into());
    }
    Ok(rows
        .into_iter()
        .map(|(attempt, request, producing_call)| {
            ToolResultAttemptCorrelation::new(
                signalbox_domain::ToolAttemptId::from_uuid(attempt),
                signalbox_domain::ToolRequestId::from_uuid(request),
                ModelCallId::from_uuid(producing_call),
            )
        })
        .collect())
}

/// Restores an availability predecessor frontier and its observation-boundary relocation.
///
/// A successor attempt owns no call yet, and its predecessor is terminal, so
/// the live call set omits it and reconstitution would fall back to the turn's
/// starting snapshot. A `switch_now` after a tool continuation would then
/// prepare the replacement without the assistant tool use or its results, and a
/// predecessor that consumed steering would reconstitute without the durable
/// consumed-steering rows the frontier holds.
///
/// When neither the predecessor nor a relocation extends the starting frontier,
/// the ordinary starting-snapshot path applies.
async fn load_availability_predecessor_snapshot(
    connection: &mut PgConnection,
    session: SessionId,
    attempt: TurnAttemptId,
    starting_frontier: signalbox_domain::ContextFrontierId,
) -> Result<Option<ResolvedContextFrontierReconstitutionInput>, ModelCallRepositoryError> {
    let frontier: Option<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(relocation.context_frontier_id, predecessor.context_frontier_id)
           FROM credential_pool_availability_successor AS successor
           JOIN model_call AS predecessor
             ON predecessor.model_call_id = successor.predecessor_model_call_id
           LEFT JOIN LATERAL (
                SELECT boundary.context_frontier_id
                  FROM runner_placement_boundary AS boundary
                  JOIN context_frontier AS frontier
                    ON frontier.owning_session_id = boundary.session_id
                   AND frontier.context_frontier_id = boundary.context_frontier_id
                 WHERE boundary.session_id = predecessor.session_id
                   AND frontier.prefix_context_frontier_id = predecessor.context_frontier_id
                 ORDER BY boundary.placement_revision DESC LIMIT 1
           ) AS relocation ON true
          WHERE successor.successor_turn_attempt_id = $1",
    )
    .bind(attempt.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(frontier) = frontier.map(signalbox_domain::ContextFrontierId::from_uuid) else {
        return Ok(None);
    };
    if frontier == starting_frontier {
        return Ok(None);
    }
    load_call_snapshot(connection, session, frontier)
        .await
        .map(Some)
}

pub(crate) async fn load_call_snapshot(
    connection: &mut PgConnection,
    session: SessionId,
    frontier: signalbox_domain::ContextFrontierId,
) -> Result<ResolvedContextFrontierReconstitutionInput, ModelCallRepositoryError> {
    let declared_count = sqlx::query_scalar::<_, Decimal>(
        "SELECT member_count
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing("model-call snapshot"))?;
    let rows = sqlx::query_as::<_, (Decimal, Uuid, Uuid)>(
        "SELECT member_position, source_session_id, semantic_entry_id
           FROM resolve_context_frontier_members($1, $2)
          ORDER BY member_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(frontier.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let actual_count = u64::try_from(rows.len())
        .map_err(|_| ModelCallCorruption::Inconsistent("model-call snapshot member count"))?;
    if declared_count != Decimal::from(actual_count) {
        return Err(ModelCallCorruption::Inconsistent("model-call snapshot member count").into());
    }
    let ordered_entries = rows
        .into_iter()
        .enumerate()
        .map(|(index, (position, source_session, semantic_entry))| {
            let expected_position = u64::try_from(index + 1).map_err(|_| {
                ModelCallCorruption::Inconsistent("model-call snapshot member positions")
            })?;
            if position != Decimal::from(expected_position) {
                return Err(ModelCallCorruption::Inconsistent(
                    "model-call snapshot member positions",
                )
                .into());
            }
            Ok(SemanticTranscriptEntryRef::from_source(
                session_id_from_uuid(source_session),
                signalbox_domain::SemanticTranscriptEntryId::from_uuid(semantic_entry),
            ))
        })
        .collect::<Result<Vec<_>, ModelCallRepositoryError>>()?;
    Ok(ResolvedContextFrontierReconstitutionInput::new(
        session,
        frontier,
        ordered_entries,
    ))
}
