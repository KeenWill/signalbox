use super::transcript_types::{
    ProcessAttachmentPreparationFailureCause, ProcessModelCallInputTokenSemantics,
    ProcessModelCallTokenUsage, ProcessModelCallUsageProvenance,
    ProcessProviderModelCallFailureCause, ProcessTranscriptModelCallUsage, ProcessTranscriptTurn,
};
use super::{
    ProcessReadCorruption, ProcessReadError, decode_nonnegative, decode_positive, required,
};
use crate::mapping::{
    defaults_version_from_numeric, model_change_adjustments_from_json, model_settings_from_json,
    model_settings_overlay_from_json, session_id_to_uuid,
};
use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, DirectModelSelection, FrozenAliasDefinition,
    FrozenModelSelection, ModelAlias, ModelCallId, ModelSelectionRequest, ProviderModelIdentity,
    ResolvedProviderTarget, SessionId, ToolRequestId, TurnId, TurnModelSettingsResolved,
    UserContent,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{Postgres, Row, Transaction};

pub(super) struct DecodedTurn {
    pub(super) turn: ProcessTranscriptTurn,
    pub(super) start_lineage: Option<DecodedStartLineage>,
    pub(super) latest_frontier: Option<ContextFrontierId>,
}

#[derive(Debug)]
pub(super) enum DecodedTurnOrigin {
    AcceptedInput {
        accepted_input: AcceptedInputId,
        content: UserContent,
    },
    DelegatedTask {
        spawning_request: ToolRequestId,
        parent_session: SessionId,
        parent_turn: TurnId,
        content: String,
    },
    DelegationWake {
        first_delivery_sequence: u64,
        through_delivery_sequence: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DecodedStartLineage {
    FirstInSession,
    After(TurnId),
}

pub(super) async fn load_execution_lineage_tip(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<Option<TurnId>, ProcessReadError> {
    let row = sqlx::query(
        "WITH RECURSIVE
            started AS (
                SELECT
                    turn_id,
                    start_lineage_kind,
                    immediate_predecessor_turn_id
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND state_kind IN ('active', 'terminal')
                   AND start_lineage_kind IS NOT NULL
            ),
            chain(turn_id) AS (
                SELECT turn_id
                  FROM started
                 WHERE start_lineage_kind = 'first_in_session'
                UNION
                SELECT child.turn_id
                  FROM started AS child
                  JOIN chain AS predecessor
                    ON child.start_lineage_kind = 'after'
                   AND child.immediate_predecessor_turn_id = predecessor.turn_id
            ),
            tips AS (
                SELECT candidate.turn_id
                  FROM started AS candidate
                 WHERE NOT EXISTS (
                    SELECT 1
                      FROM started AS successor
                     WHERE successor.start_lineage_kind = 'after'
                       AND successor.immediate_predecessor_turn_id = candidate.turn_id
                 )
            )
         SELECT
            (SELECT count(*) FROM started) AS started_count,
            (SELECT count(*) FROM started
              WHERE start_lineage_kind = 'first_in_session') AS root_count,
            (SELECT count(*) FROM chain) AS visited_count,
            (SELECT count(*) FROM tips) AS tip_count,
            EXISTS (
                SELECT 1
                  FROM started
                 WHERE start_lineage_kind = 'after'
                 GROUP BY immediate_predecessor_turn_id
                HAVING count(*) > 1
            ) AS branched,
            EXISTS (
                SELECT 1
                  FROM started AS child
                  LEFT JOIN started AS predecessor
                    ON predecessor.turn_id = child.immediate_predecessor_turn_id
                 WHERE child.start_lineage_kind = 'after'
                   AND predecessor.turn_id IS NULL
            ) AS missing_predecessor,
            (SELECT turn_id FROM tips LIMIT 1) AS tip_turn_id",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    decode_execution_lineage_tip(
        decode_database_count(&row, "started_count", "started turn count")?,
        decode_database_count(&row, "root_count", "root turn count")?,
        decode_database_count(&row, "visited_count", "visited turn count")?,
        decode_database_count(&row, "tip_count", "tip turn count")?,
        row.try_get("branched")?,
        row.try_get("missing_predecessor")?,
        row.try_get::<Option<Uuid>, _>("tip_turn_id")?
            .map(TurnId::from_uuid),
    )
}

pub(super) fn decode_execution_lineage_tip(
    started_count: u64,
    root_count: u64,
    visited_count: u64,
    tip_count: u64,
    branched: bool,
    missing_predecessor: bool,
    tip: Option<TurnId>,
) -> Result<Option<TurnId>, ProcessReadError> {
    if started_count == 0 {
        return if root_count == 0
            && visited_count == 0
            && tip_count == 0
            && !branched
            && !missing_predecessor
            && tip.is_none()
        {
            Ok(None)
        } else {
            Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into())
        };
    }
    if root_count != 1
        || visited_count != started_count
        || tip_count != 1
        || branched
        || missing_predecessor
    {
        return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
    }
    tip.map(Some)
        .ok_or_else(|| ProcessReadCorruption::Inconsistent("turn execution lineage").into())
}

pub(super) async fn load_transcript_turn_count(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<u64, ProcessReadError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turn_lifecycle AS turn
          WHERE turn.session_id = $1
            AND (
                goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id)
                OR EXISTS (
                    SELECT 1
                      FROM session_delegation_logical_terminal AS logical_terminal
                     WHERE logical_terminal.child_session_id = turn.session_id
                       AND logical_terminal.child_turn_id = turn.turn_id
                )
            )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    u64::try_from(count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript turn count").into())
}

pub(super) async fn load_terminal_model_call_count(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
) -> Result<u64, ProcessReadError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT
            (SELECT count(*)
               FROM model_call
              WHERE session_id = $1
                AND state_kind = 'terminal')
            +
            (SELECT count(*)
               FROM tool_approval_judge_model_call
              WHERE session_id = $1
                AND state_kind = 'terminal')",
    )
    .bind(session_id_to_uuid(session))
    .fetch_one(&mut **transaction)
    .await?;
    u64::try_from(count)
        .map_err(|_| ProcessReadCorruption::InvalidOrdinal("transcript model-call count").into())
}

pub(super) async fn load_next_model_call_usage(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    after: Option<(u64, ModelCallId)>,
) -> Result<Option<PgRow>, ProcessReadError> {
    sqlx::query(
        "WITH terminal_call AS (
            SELECT turn_id, session_id, model_call_id,
                   resolved_provider_model_identity_id, credential_reference,
                   usage_provenance_kind, usage_input_includes_cache_tokens,
                   usage_input_tokens, usage_output_tokens,
                   usage_cache_creation_input_tokens,
                   usage_cache_read_input_tokens
              FROM model_call
             WHERE session_id = $1 AND state_kind = 'terminal'
            UNION ALL
            SELECT turn_id, session_id, model_call_id,
                   resolved_provider_model_identity_id, credential_reference,
                   usage_provenance_kind, usage_input_includes_cache_tokens,
                   input_tokens, output_tokens,
                   cache_creation_input_tokens, cache_read_input_tokens
              FROM tool_approval_judge_model_call
             WHERE session_id = $1 AND state_kind = 'terminal'
         )
         SELECT
            turn.acceptance_position,
            call.turn_id,
            call.model_call_id,
            call.resolved_provider_model_identity_id,
            call.credential_reference,
            call.usage_provenance_kind,
            call.usage_input_includes_cache_tokens,
            call.usage_input_tokens,
            call.usage_output_tokens,
            call.usage_cache_creation_input_tokens,
            call.usage_cache_read_input_tokens
           FROM terminal_call AS call
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = call.turn_id
            AND turn.session_id = call.session_id
          WHERE $2::numeric IS NULL
             OR turn.acceptance_position > $2
             OR (
                 turn.acceptance_position = $2
                 AND call.model_call_id > $3
             )
          ORDER BY turn.acceptance_position, call.model_call_id
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .bind(after.map(|(position, _)| Decimal::from(position)))
    .bind(after.map(|(_, call)| call.into_uuid()))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(Into::into)
}

pub(super) fn decode_model_call_usage(
    row: &PgRow,
) -> Result<(u64, ProcessTranscriptModelCallUsage), ProcessReadError> {
    let acceptance_position = decode_positive(
        required(row, "acceptance_position")?,
        "model-call turn acceptance position",
    )?;
    let provenance_value = required::<String>(row, "usage_provenance_kind")?;
    let Some(provenance) = ProcessModelCallUsageProvenance::from_storage(provenance_value.as_str())
    else {
        return Err(ProcessReadCorruption::Unsupported {
            field: "usage_provenance_kind",
            value: provenance_value,
        }
        .into());
    };
    let input_tokens = row
        .try_get::<Option<Decimal>, _>("usage_input_tokens")?
        .map(|value| decode_nonnegative(value, "model-call input tokens"))
        .transpose()?;
    let output_tokens = row
        .try_get::<Option<Decimal>, _>("usage_output_tokens")?
        .map(|value| decode_nonnegative(value, "model-call output tokens"))
        .transpose()?;
    let cache_creation_input_tokens = row
        .try_get::<Option<Decimal>, _>("usage_cache_creation_input_tokens")?
        .map(|value| decode_nonnegative(value, "model-call cache-creation input tokens"))
        .transpose()?;
    let cache_read_input_tokens = row
        .try_get::<Option<Decimal>, _>("usage_cache_read_input_tokens")?
        .map(|value| decode_nonnegative(value, "model-call cache-read input tokens"))
        .transpose()?;
    Ok((
        acceptance_position,
        ProcessTranscriptModelCallUsage {
            turn: TurnId::from_uuid(required(row, "turn_id")?),
            call: ModelCallId::from_uuid(required(row, "model_call_id")?),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
                row,
                "resolved_provider_model_identity_id",
            )?)),
            credential_profile: required(row, "credential_reference")?,
            input_token_semantics: ProcessModelCallInputTokenSemantics::from_storage(
                row.try_get("usage_input_includes_cache_tokens")?,
            ),
            provenance,
            usage: ProcessModelCallTokenUsage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
            },
        },
    ))
}

pub(super) async fn load_next_transcript_turn(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    after: Option<u64>,
) -> Result<Option<PgRow>, ProcessReadError> {
    sqlx::query(
        "SELECT
            turn.turn_id,
            turn.session_id AS turn_session_id,
            turn.acceptance_position,
            turn.origin_kind,
            turn.origin_accepted_input_id,
            turn.state_kind,
            turn.start_lineage_kind,
            turn.immediate_predecessor_turn_id,
            turn.starting_frontier_id,
            turn.terminal_frontier_id,
            turn.active_phase_kind,
            credential_wait.wait_attempt_id AS credential_wait_attempt_id,
            credential_wait.frontier_id AS credential_wait_frontier_id,
            credential_wait.cause AS credential_wait_cause,
            turn.child_wait_request_id,
            turn.current_attempt_id,
            turn.terminal_disposition_kind,
            turn.recovery_model_call_id,
            turn.active_tool_round_call_id,
            turn.approval_tool_request_id,
            turn.recovery_tool_attempt_id,
            turn.runner_recovery_runner_id,
            turn.runner_recovery_placement_revision,
            turn.runner_recovery_tool_attempt_id,
            turn.terminal_attempt_id,
            turn.terminal_model_call_id,
            turn.terminal_tool_attempt_id,
            terminal_call.terminal_disposition_kind
                AS terminal_model_call_disposition_kind,
            terminal_call.terminal_provider_failure_cause
                AS terminal_model_call_provider_failure_cause,
            terminal_call.terminal_attachment_preparation_failure_cause
                AS terminal_model_call_attachment_preparation_failure_cause,
            accepted.accepted_input_id,
            accepted.acceptance_position AS accepted_position,
            accepted.origin_turn_id,
            CASE WHEN accepted.accepted_input_id IS NULL THEN NULL
                 ELSE accepted_input_content_parts_json(
                    accepted.accepted_input_id)
            END AS accepted_content,
            task.spawning_tool_request_id AS delegated_spawning_tool_request_id,
            task.task_content AS delegated_task_content,
            relation.parent_session_id AS delegated_parent_session_id,
            relation.parent_turn_id AS delegated_parent_turn_id,
            wake.first_delivery_sequence AS delegated_wake_first_delivery_sequence,
            wake.through_delivery_sequence AS delegated_wake_through_delivery_sequence,
            child_wait.spawning_tool_request_id AS child_wait_spawning_request_id,
            child_wait.child_session_id AS child_wait_child_session_id,
            accepted.model_settings_override AS accepted_model_settings_override,
            settings.accepted_input_id AS settings_accepted_input_id,
            settings.turn_id AS settings_turn_id,
            settings.session_id AS settings_session_id,
            settings.defaults_version AS settings_defaults_version,
            settings.selected_direct_model_id AS settings_selected_direct_id,
            settings.per_call_model_settings AS settings_per_call_model_settings,
            settings.resolved_model_settings AS settings_resolved_model_settings,
            settings.adjusted_from_selection_id AS settings_adjusted_from_selection_id,
            settings.adjustments AS settings_adjustments,
            configuration_origin.defaults_version AS origin_defaults_version,
            configuration_origin.requested_model_kind AS origin_requested_model_kind,
            configuration_origin.requested_direct_model_selection_id
                AS origin_requested_direct_id,
            configuration_origin.requested_model_alias_id AS origin_requested_alias_id,
            configuration_origin.frozen_model_kind AS origin_frozen_model_kind,
            configuration_origin.frozen_direct_model_selection_id AS origin_frozen_direct_id,
            configuration_origin.frozen_model_alias_id AS origin_frozen_alias_id,
            configuration_origin.frozen_alias_selected_direct_id
                AS origin_frozen_alias_selected_direct_id,
            configuration_origin.model_settings_evidence_required
                AS origin_model_settings_evidence_required,
            origin_accepted.model_settings_override
                AS origin_model_settings_override,
            origin_defaults.model_settings AS origin_defaults_model_settings,
            current_call.model_call_id AS current_model_call_id,
            current_call.state_kind AS current_model_call_state_kind,
            current_call.context_frontier_id AS current_model_call_frontier_id,
            recovery_call.context_frontier_id AS recovery_model_call_frontier_id,
            automatic_reconciliation.state_kind
                AS automatic_reconciliation_state_kind,
            automatic_reconciliation.attempt_count
                AS automatic_reconciliation_attempt_count,
            automatic_reconciliation.model_call_id
                AS automatic_reconciliation_model_call_id,
            automatic_reconciliation.tool_attempt_id
                AS automatic_reconciliation_tool_attempt_id,
            active_tool_round.boundary_frontier_id AS active_tool_round_frontier_id,
            logical_terminal.spawning_tool_request_id
                AS logical_terminal_spawning_request_id,
            logical_terminal.terminal_frontier_id
                AS logical_terminal_frontier_id,
            logical_terminal_event.outcome_kind
                AS logical_terminal_outcome_kind,
            logical_terminal_event.reason_kind
                AS logical_terminal_reason_kind,
            logical_terminal_event.provenance_kind AS provenance_kind,
            logical_terminal_event.provenance_session_id AS provenance_session_id,
            logical_terminal_event.provenance_turn_id AS provenance_turn_id,
            logical_terminal_event.provenance_goal_generation
                AS provenance_goal_generation,
            logical_terminal_event.provenance_command_id AS provenance_command_id
           FROM turn_lifecycle AS turn
           LEFT JOIN accepted_input AS accepted
             ON accepted.accepted_input_id = turn.origin_accepted_input_id
            AND accepted.session_id = turn.session_id
           LEFT JOIN session_delegation_initial_task AS task
             ON task.turn_id = turn.turn_id
            AND task.child_session_id = turn.session_id
           LEFT JOIN session_delegation_wake_turn_origin AS wake
             ON wake.turn_id = turn.turn_id
            AND wake.recipient_session_id = turn.session_id
            AND wake.admission_position = turn.acceptance_position
           LEFT JOIN session_delegation AS relation
             ON relation.spawning_tool_request_id = task.spawning_tool_request_id
            AND relation.child_session_id = task.child_session_id
           LEFT JOIN session_delegation_wait AS child_wait
             ON child_wait.awaiting_tool_request_id = turn.child_wait_request_id
            AND child_wait.parent_turn_id = turn.turn_id
            AND child_wait.parent_session_id = turn.session_id
            AND child_wait.wait_mode = 'foreground'
           LEFT JOIN turn_model_settings_resolved AS settings
             ON settings.accepted_input_id = turn.origin_accepted_input_id
            AND settings.turn_id = turn.turn_id
            AND settings.session_id = turn.session_id
           LEFT JOIN LATERAL (
                WITH RECURSIVE configuration_chain AS (
                    SELECT queued.*
                      FROM queued_input_origin AS queued
                     WHERE queued.accepted_input_id = turn.origin_accepted_input_id
                       AND queued.turn_id = turn.turn_id
                       AND queued.session_id = turn.session_id
                    UNION
                    SELECT source.*
                      FROM configuration_chain AS current
                      JOIN queued_input_origin AS source
                        ON source.turn_id = current.source_configuration_turn_id
                       AND source.session_id = current.session_id
                )
                SELECT *
                  FROM configuration_chain
                 WHERE source_configuration_turn_id IS NULL
           ) AS configuration_origin ON TRUE
           LEFT JOIN accepted_input AS origin_accepted
             ON origin_accepted.accepted_input_id =
                configuration_origin.accepted_input_id
            AND origin_accepted.session_id = configuration_origin.session_id
            AND origin_accepted.origin_turn_id = configuration_origin.turn_id
           LEFT JOIN session_defaults_version AS origin_defaults
             ON origin_defaults.session_id = configuration_origin.session_id
            AND origin_defaults.version = configuration_origin.defaults_version
           LEFT JOIN model_call AS current_call
             ON current_call.turn_attempt_id = turn.current_attempt_id
            AND current_call.turn_id = turn.turn_id
            AND current_call.session_id = turn.session_id
            AND current_call.state_kind <> 'terminal'
           LEFT JOIN model_call AS recovery_call
             ON recovery_call.model_call_id = turn.recovery_model_call_id
            AND recovery_call.turn_attempt_id = turn.current_attempt_id
            AND recovery_call.turn_id = turn.turn_id
            AND recovery_call.session_id = turn.session_id
            AND recovery_call.state_kind = 'terminal'
           LEFT JOIN automatic_reconciliation AS automatic_reconciliation
             ON automatic_reconciliation.turn_id = turn.turn_id
            AND automatic_reconciliation.session_id = turn.session_id
           LEFT JOIN credential_availability_wait AS credential_wait
             ON credential_wait.turn_id = turn.turn_id AND credential_wait.session_id = turn.session_id
            AND credential_wait.consumed_by_attempt_id IS NULL
           LEFT JOIN model_call AS terminal_call
             ON terminal_call.model_call_id = turn.terminal_model_call_id
            AND terminal_call.turn_attempt_id = turn.terminal_attempt_id
            AND terminal_call.turn_id = turn.turn_id
            AND terminal_call.session_id = turn.session_id
            AND terminal_call.state_kind = 'terminal'
           LEFT JOIN tool_round AS active_tool_round
             ON active_tool_round.producing_model_call_id =
                turn.active_tool_round_call_id
            AND active_tool_round.turn_id = turn.turn_id
            AND active_tool_round.session_id = turn.session_id
           LEFT JOIN session_delegation_logical_terminal AS logical_terminal
             ON logical_terminal.child_session_id = turn.session_id
            AND logical_terminal.child_turn_id = turn.turn_id
           LEFT JOIN session_delegation_event AS logical_terminal_event
             ON logical_terminal_event.spawning_tool_request_id =
                    logical_terminal.spawning_tool_request_id
            AND logical_terminal_event.event_kind = 'outcome_recorded'
            AND logical_terminal_event.provenance_command_id =
                    logical_terminal.root_command_id
            AND logical_terminal_event.outcome_kind = CASE
                    logical_terminal.disposition_kind
                    WHEN 'stopped' THEN 'child_stopped'
                    WHEN 'cancelled' THEN 'child_cancelled'
                END
          WHERE turn.session_id = $1
            AND (
                goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id)
                OR logical_terminal.child_turn_id IS NOT NULL
            )
            AND ($2::numeric IS NULL OR turn.acceptance_position > $2)
          ORDER BY turn.acceptance_position
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .bind(after.map(Decimal::from))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(Into::into)
}

pub(super) fn decode_provider_failure_cause(
    value: &str,
) -> Result<ProcessProviderModelCallFailureCause, ProcessReadError> {
    match value {
        "credential_rejected" => Ok(ProcessProviderModelCallFailureCause::CredentialRejected),
        "permission_denied" => Ok(ProcessProviderModelCallFailureCause::PermissionDenied),
        "invalid_request" => Ok(ProcessProviderModelCallFailureCause::InvalidRequest),
        "target_not_found" => Ok(ProcessProviderModelCallFailureCause::TargetNotFound),
        "request_too_large" => Ok(ProcessProviderModelCallFailureCause::RequestTooLarge),
        "rate_limited" => Ok(ProcessProviderModelCallFailureCause::RateLimited),
        "quota_exhausted" => Ok(ProcessProviderModelCallFailureCause::QuotaExhausted),
        "overloaded" => Ok(ProcessProviderModelCallFailureCause::Overloaded),
        "provider_internal" => Ok(ProcessProviderModelCallFailureCause::ProviderInternal),
        "unrecognized" => Ok(ProcessProviderModelCallFailureCause::Unrecognized),
        value => Err(ProcessReadCorruption::Unsupported {
            field: "model-call provider failure cause",
            value: value.to_owned(),
        }
        .into()),
    }
}

pub(super) fn decode_attachment_preparation_failure_cause(
    value: &str,
) -> Result<ProcessAttachmentPreparationFailureCause, ProcessReadError> {
    match value {
        "too_large" => Ok(ProcessAttachmentPreparationFailureCause::TooLarge),
        "missing" => Ok(ProcessAttachmentPreparationFailureCause::Missing),
        "corrupt" => Ok(ProcessAttachmentPreparationFailureCause::Corrupt),
        value => Err(ProcessReadCorruption::Unsupported {
            field: "model-call attachment-preparation failure cause",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_database_count(
    row: &PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<u64, ProcessReadError> {
    let count: i64 = row.try_get(column)?;
    u64::try_from(count).map_err(|_| ProcessReadCorruption::InvalidOrdinal(field).into())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_transcript_turn_origin(
    origin_kind: String,
    origin_accepted_input: Option<Uuid>,
    accepted_input: Option<Uuid>,
    accepted_position: Option<Decimal>,
    accepted_origin: Option<Uuid>,
    accepted_content: Option<Value>,
    delegated_spawning_request: Option<Uuid>,
    delegated_parent_session: Option<Uuid>,
    delegated_parent_turn: Option<Uuid>,
    delegated_task_content: Option<String>,
    delegated_wake_first: Option<Decimal>,
    delegated_wake_through: Option<Decimal>,
    turn: TurnId,
    acceptance_position: u64,
) -> Result<DecodedTurnOrigin, ProcessReadError> {
    match (
        origin_kind.as_str(),
        origin_accepted_input,
        accepted_input,
        accepted_position,
        accepted_origin,
        accepted_content,
        delegated_spawning_request,
        delegated_parent_session,
        delegated_parent_turn,
        delegated_task_content,
        delegated_wake_first,
        delegated_wake_through,
    ) {
        (
            "accepted_input",
            Some(origin_accepted_input),
            Some(accepted_input),
            Some(accepted_position),
            Some(accepted_origin),
            Some(content),
            None,
            None,
            None,
            None,
            None,
            None,
        ) => {
            let accepted_position = decode_positive(accepted_position, "accepted input position")?;
            let content = crate::user_content::decode(content)
                .map_err(|_| ProcessReadCorruption::Inconsistent("turn accepted-input content"))?;
            if origin_accepted_input != accepted_input
                || accepted_position != acceptance_position
                || accepted_origin != turn.into_uuid()
            {
                return Err(
                    ProcessReadCorruption::Inconsistent("turn accepted-input correlation").into(),
                );
            }
            Ok(DecodedTurnOrigin::AcceptedInput {
                accepted_input: AcceptedInputId::from_uuid(accepted_input),
                content,
            })
        }
        (
            "delegation",
            None,
            None,
            None,
            None,
            None,
            Some(spawning_request),
            Some(parent_session),
            Some(parent_turn),
            Some(content),
            None,
            None,
        ) if !content.is_empty() => Ok(DecodedTurnOrigin::DelegatedTask {
            spawning_request: ToolRequestId::from_uuid(spawning_request),
            parent_session: SessionId::from_uuid(parent_session),
            parent_turn: TurnId::from_uuid(parent_turn),
            content,
        }),
        (
            "delegation",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(first),
            Some(through),
        ) => {
            let first = decode_positive(first, "delegation wake first delivery sequence")?;
            let through = decode_positive(through, "delegation wake through delivery sequence")?;
            if first > through {
                return Err(
                    ProcessReadCorruption::Inconsistent("delegation wake delivery range").into(),
                );
            }
            Ok(DecodedTurnOrigin::DelegationWake {
                first_delivery_sequence: first,
                through_delivery_sequence: through,
            })
        }
        ("accepted_input" | "delegation", ..) => {
            Err(ProcessReadCorruption::Inconsistent("turn origin correlation").into())
        }
        (value, ..) => Err(ProcessReadCorruption::Unsupported {
            field: "turn origin kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn decode_transcript_model_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
) -> Result<ModelSelectionRequest, ProcessReadError> {
    match (kind.as_str(), direct, alias) {
        ("direct", Some(selection), None) => Ok(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("alias", None, Some(alias)) => {
            Ok(ModelSelectionRequest::Alias(ModelAlias::from_uuid(alias)))
        }
        ("direct" | "alias", _, _) => {
            Err(ProcessReadCorruption::Inconsistent("turn requested model shape").into())
        }
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "turn requested model kind",
            value: kind,
        }
        .into()),
    }
}

fn decode_transcript_frozen_model(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
) -> Result<FrozenModelSelection, ProcessReadError> {
    match (kind.as_str(), direct, alias, alias_selected) {
        ("direct", Some(selection), None, None) => Ok(FrozenModelSelection::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("frozen_alias", None, Some(alias), Some(selected)) => {
            Ok(FrozenModelSelection::FrozenAlias {
                alias: ModelAlias::from_uuid(alias),
                definition: FrozenAliasDefinition::selecting(DirectModelSelection::from_uuid(
                    selected,
                )),
            })
        }
        ("direct" | "frozen_alias", _, _, _) => {
            Err(ProcessReadCorruption::Inconsistent("turn frozen model shape").into())
        }
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "turn frozen model kind",
            value: kind,
        }
        .into()),
    }
}

fn requested_from_transcript_frozen(selection: &FrozenModelSelection) -> ModelSelectionRequest {
    match selection {
        FrozenModelSelection::Direct(selection) => ModelSelectionRequest::Direct(*selection),
        FrozenModelSelection::FrozenAlias { alias, .. } => ModelSelectionRequest::Alias(*alias),
    }
}

pub(super) fn decode_transcript_turn_model_settings(
    row: &PgRow,
    turn: TurnId,
    accepted_input: AcceptedInputId,
) -> Result<Option<TurnModelSettingsResolved>, ProcessReadError> {
    let stored_accepted: Option<Uuid> = row.try_get("settings_accepted_input_id")?;
    let stored_turn: Option<Uuid> = row.try_get("settings_turn_id")?;
    let stored_session: Option<Uuid> = row.try_get("settings_session_id")?;
    let stored_defaults: Option<Decimal> = row.try_get("settings_defaults_version")?;
    let stored_selected: Option<Uuid> = row.try_get("settings_selected_direct_id")?;
    let stored_per_call: Option<Value> = row.try_get("settings_per_call_model_settings")?;
    let stored_settings: Option<Value> = row.try_get("settings_resolved_model_settings")?;
    let stored_adjustments: Option<Value> = row.try_get("settings_adjustments")?;
    let absent = stored_accepted.is_none()
        && stored_turn.is_none()
        && stored_session.is_none()
        && stored_defaults.is_none()
        && stored_selected.is_none()
        && stored_per_call.is_none()
        && stored_settings.is_none()
        && stored_adjustments.is_none();
    if absent {
        let evidence_required: bool = required(row, "origin_model_settings_evidence_required")?;
        return if evidence_required {
            Err(ProcessReadCorruption::Missing("turn model settings evidence").into())
        } else {
            Ok(None)
        };
    }
    let (Some(stored_accepted), Some(stored_turn), Some(stored_session), Some(stored_defaults)) = (
        stored_accepted,
        stored_turn,
        stored_session,
        stored_defaults,
    ) else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_selected) = stored_selected else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_per_call) = stored_per_call else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_settings) = stored_settings else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let Some(stored_adjustments) = stored_adjustments else {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings shape").into());
    };
    let turn_session: Uuid = required(row, "turn_session_id")?;
    if AcceptedInputId::from_uuid(stored_accepted) != accepted_input
        || TurnId::from_uuid(stored_turn) != turn
        || stored_session != turn_session
    {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings identity").into());
    }
    let defaults_version = defaults_version_from_numeric(stored_defaults)
        .map_err(|_| ProcessReadCorruption::Inconsistent("turn model settings version"))?;
    let origin_defaults = defaults_version_from_numeric(required(row, "origin_defaults_version")?)
        .map_err(|_| ProcessReadCorruption::Inconsistent("turn origin defaults version"))?;
    let requested = decode_transcript_model_selection(
        required(row, "origin_requested_model_kind")?,
        row.try_get("origin_requested_direct_id")?,
        row.try_get("origin_requested_alias_id")?,
    )?;
    let frozen = decode_transcript_frozen_model(
        required(row, "origin_frozen_model_kind")?,
        row.try_get("origin_frozen_direct_id")?,
        row.try_get("origin_frozen_alias_id")?,
        row.try_get("origin_frozen_alias_selected_direct_id")?,
    )?;
    let per_call = model_settings_overlay_from_json(stored_per_call)
        .map_err(|_| ProcessReadCorruption::Inconsistent("turn per-call model settings"))?;
    let origin_per_call =
        model_settings_overlay_from_json(required(row, "origin_model_settings_override")?)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn accepted model settings"))?;
    if defaults_version != origin_defaults
        || requested != requested_from_transcript_frozen(&frozen)
        || frozen.selected_direct().into_uuid() != stored_selected
        || per_call != origin_per_call
    {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings origin").into());
    }
    let event = TurnModelSettingsResolved::try_new(
        accepted_input,
        turn,
        defaults_version,
        frozen,
        per_call,
        model_settings_from_json(stored_settings)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn resolved model settings"))?,
        row.try_get::<Option<Uuid>, _>("settings_adjusted_from_selection_id")?
            .map(DirectModelSelection::from_uuid),
        model_change_adjustments_from_json(stored_adjustments)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn model setting adjustments"))?,
    )
    .ok_or(ProcessReadCorruption::Inconsistent(
        "turn model settings evidence",
    ))?;
    let origin_defaults =
        model_settings_from_json(required(row, "origin_defaults_model_settings")?)
            .map_err(|_| ProcessReadCorruption::Inconsistent("turn defaults model settings"))?;
    if !crate::model_settings_resolution::matches_defaults(&event, origin_defaults) {
        return Err(ProcessReadCorruption::Inconsistent("turn model settings defaults").into());
    }
    Ok(Some(event))
}

pub(super) fn admitted_automatic_reconciliation_attempts(
    attempts: i32,
    exhausted: bool,
    budget: Option<Option<u32>>,
) -> Result<u32, ProcessReadError> {
    let attempts = u32::try_from(attempts).map_err(|_| {
        ProcessReadCorruption::Inconsistent("automatic reconciliation attempt count")
    })?;
    let admitted = if exhausted {
        match budget {
            Some(Some(budget)) => attempts == budget,
            Some(None) => false,
            None => attempts > 0,
        }
    } else {
        budget.is_none_or(|budget| budget.is_none_or(|budget| attempts <= budget))
    };
    admitted
        .then_some(attempts)
        .ok_or(ProcessReadCorruption::Inconsistent(
            "automatic reconciliation attempt budget",
        ))
        .map_err(Into::into)
}
