use super::decode::decode_complete;
use super::{
    APPLIED, REJECTED, StoredAutomaticReconciliationAuthority, StoredTerminalTurnDisposition,
    StoredTurnOriginKey, StoredTurnOriginKind, StoredTurnOriginLink, StoredTurnOriginProvenance,
    SubmitInputCorruption, SubmitInputRepositoryError, decode_content, decode_position, required,
    turn_origin_dependency_order,
};
use crate::mapping::{
    accepted_input_id_from_uuid, durable_command_id_from_uuid, durable_command_id_to_uuid,
    positive_u64_from_numeric, session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid,
    turn_id_to_uuid,
};
use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputQueuePriority, AppliedInterruptCommandResult, DeliveryRequest, DurableCommandId,
    GoalEventOrdinal, GoalGeneration, GoalTurnOriginConstructionInput, GoalTurnSource,
    IssuedOperationRef, ModelCallId, NonAcceptedTurnPredecessorReconstitutionInput,
    ReconstitutedSubmitInput, SessionConfigurationDefaultsVersion, SteeringReclassificationReason,
    SubmitInputAppliedResult, SubmitInputAutomaticReconciliationConstructionInput,
    SubmitInputDirectTurnOriginConstructionInput,
    SubmitInputInterruptedModelCallReconciliationConstructionInput,
    SubmitInputInterruptedToolReconciliationConstructionInput,
    SubmitInputReclassifiedTurnOriginConstructionInput, SubmitInputResult,
    SubmitInputTerminalSourceConstructionInput, SubmitInputTerminalSourceReconstitutionInput,
    SubmitInputTurnOriginReconstitutionInput, ToolAttemptId,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};

pub(super) fn configured_defaults_version(
    delivery: DeliveryRequest,
) -> Option<SessionConfigurationDefaultsVersion> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => {
            Some(configuration.expected_session_defaults_version())
        }
        DeliveryRequest::NextSafePoint { .. } => None,
    }
}

pub(super) fn configured_model_settings(
    delivery: DeliveryRequest,
) -> Option<signalbox_domain::ModelSettingsOverlay> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => {
            Some(configuration.model_settings())
        }
        DeliveryRequest::NextSafePoint { .. } => None,
    }
}

pub(super) async fn load_complete_rows(
    connection: &mut PgConnection,
    command_ids: &[Uuid],
) -> Result<Vec<PgRow>, SubmitInputRepositoryError> {
    let rows = sqlx::query(
        "SELECT
            registry.command_id AS registry_command_id,
            registry.command_kind AS registry_kind,
            registry.storage_version AS registry_version,
            typed.command_id AS typed_command_id,
            typed.command_kind AS typed_kind,
            typed.storage_version AS typed_version,
            typed.session_id AS command_session_id,
            typed.actor_kind,
            typed.actor_turn_id,
            typed.actor_tool_request_id,
            typed.actor_program_run_id,
            actor_program.run_id AS verified_actor_program_run_id,
            (
                SELECT COALESCE(
                    jsonb_agg(
                        jsonb_build_object(
                            'position', part.position,
                            'part_kind', part.part_kind,
                            'text_value', part.text_value,
                            'blob_digest', CASE
                                WHEN part.blob_digest IS NULL THEN NULL
                                ELSE 'sha256:' || encode(part.blob_digest, 'hex')
                            END,
                            'attachment_kind', part.attachment_kind,
                            'declared_media_type', part.declared_media_type,
                            'display_filename', part.display_filename
                        ) ORDER BY part.position
                    ),
                    '[]'::jsonb
                )
                  FROM submit_input_command_content_part AS part
                 WHERE part.command_id = typed.command_id
            ) AS command_content_parts,
            typed.delivery_kind AS command_delivery_kind,
            typed.descendant_scope AS command_descendant_scope,
            typed.expected_active_turn_id AS command_expected_active_turn_id,
            typed.expected_defaults_version AS command_expected_defaults_version,
            typed.model_override_kind AS command_model_override_kind,
            typed.replacement_model_kind AS command_replacement_model_kind,
            typed.replacement_direct_model_selection_id AS command_replacement_direct_id,
            typed.replacement_model_alias_id AS command_replacement_alias_id,
            typed.model_settings_override AS command_model_settings_override,
            typed.result_kind,
            typed.rejection_kind,
            typed.result_session_id,
            typed.result_accepted_input_id,
            typed.result_turn_id,
            typed.result_actual_active_turn_id,
            typed.result_expected_active_turn_id,
            typed.result_expected_defaults_version,
            typed.result_current_defaults_version,
            typed.result_unknown_alias_id,
            typed.result_selected_defaults_version,
            typed.result_last_position,
            typed.result_existing_interrupt_command_id,
            typed.result_attachment_digest,
            typed.result_attachment_verified_prefix,
            typed.result_attachment_maximum_bytes,
            accepted.accepting_command_id,
            accepted.accepted_input_id,
            accepted.session_id AS accepted_session_id,
            (
                SELECT COALESCE(
                    jsonb_agg(
                        jsonb_build_object(
                            'position', part.position,
                            'part_kind', part.part_kind,
                            'text_value', part.text_value,
                            'blob_digest', CASE
                                WHEN part.blob_digest IS NULL THEN NULL
                                ELSE 'sha256:' || encode(part.blob_digest, 'hex')
                            END,
                            'attachment_kind', part.attachment_kind,
                            'declared_media_type', part.declared_media_type,
                            'display_filename', part.display_filename
                        ) ORDER BY part.position
                    ),
                    '[]'::jsonb
                )
                  FROM accepted_input_content_part AS part
                 WHERE part.accepted_input_id = accepted.accepted_input_id
            ) AS accepted_content_parts,
            accepted.delivery_kind AS accepted_delivery_kind,
            accepted.descendant_scope AS accepted_descendant_scope,
            accepted.expected_active_turn_id AS accepted_expected_active_turn_id,
            accepted.expected_defaults_version AS accepted_expected_defaults_version,
            accepted.model_override_kind AS accepted_model_override_kind,
            accepted.replacement_model_kind AS accepted_replacement_model_kind,
            accepted.replacement_direct_model_selection_id AS accepted_replacement_direct_id,
            accepted.replacement_model_alias_id AS accepted_replacement_alias_id,
            accepted.model_settings_override AS accepted_model_settings_override,
            accepted.acceptance_position AS accepted_position,
            accepted.disposition_kind,
            accepted.origin_turn_id,
            queued.turn_id AS queued_turn_id,
            queued.accepted_input_id AS queued_accepted_input_id,
            queued.session_id AS queued_session_id,
            queued.acceptance_position AS queued_position,
            queued.priority_kind,
            queued.interrupt_predecessor_turn_id,
            non_accepted_predecessor.session_id
                AS non_accepted_predecessor_session_id,
            non_accepted_predecessor.turn_id
                AS non_accepted_predecessor_turn_id,
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
            configuration_origin.model_settings_evidence_required
                AS origin_model_settings_evidence_required,
            defaults.session_id AS defaults_session_id,
            defaults.version AS defaults_version,
            defaults.model_selection_kind AS defaults_model_kind,
            defaults.direct_model_selection_id AS defaults_direct_id,
            defaults.model_alias_id AS defaults_alias_id,
            defaults.dangerous_tool_auto_approval AS defaults_tool_auto_approval,
            defaults.model_settings AS defaults_model_settings,
            settings.selected_direct_model_id AS settings_selected_direct_id,
            settings.session_id AS settings_session_id,
            settings.defaults_version AS settings_defaults_version,
            settings.per_call_model_settings,
            settings.resolved_model_settings,
            settings.adjusted_from_selection_id,
            settings.adjustments AS model_settings_adjustments,
            (
                SELECT count(*)
                  FROM accepted_input AS effect
                 WHERE effect.accepting_command_id = typed.command_id
            ) AS accepted_effect_count,
            (
                SELECT count(*)
                  FROM queued_input_origin AS effect_queue
                  JOIN accepted_input AS effect_input
                    ON effect_input.accepted_input_id = effect_queue.accepted_input_id
                 WHERE effect_input.accepting_command_id = typed.command_id
            ) AS queued_effect_count
         FROM durable_command AS registry
         LEFT JOIN submit_input_command AS typed
           ON typed.command_id = registry.command_id
         LEFT JOIN program_run_journal_stream AS actor_program
           ON actor_program.run_id = typed.actor_program_run_id
         LEFT JOIN accepted_input AS accepted
           ON accepted.accepted_input_id = typed.result_accepted_input_id
         LEFT JOIN queued_input_origin AS queued
           ON queued.accepted_input_id = accepted.accepted_input_id
         LEFT JOIN turn_lifecycle AS non_accepted_predecessor
           ON non_accepted_predecessor.session_id = queued.session_id
          AND non_accepted_predecessor.turn_id = typed.expected_active_turn_id
          AND non_accepted_predecessor.origin_kind = 'delegation'
          AND (
                non_accepted_predecessor.state_kind = 'terminal'
                OR non_accepted_predecessor.delegation_runtime_terminal
          )
          AND typed.delivery_kind = 'interrupt'
          AND queued.priority_kind = 'interrupt_immediately_after'
          AND queued.interrupt_predecessor_turn_id =
                  non_accepted_predecessor.turn_id
          AND accepted_input_turn_queue_predecessor(
                  queued.session_id, queued.turn_id
              ) = non_accepted_predecessor.turn_id
         LEFT JOIN LATERAL (
              WITH RECURSIVE configuration_chain AS (
                  SELECT origin.*
                    FROM queued_input_origin AS origin
                   WHERE origin.turn_id = queued.turn_id
                     AND origin.session_id = queued.session_id
                  UNION
                  SELECT source.*
                    FROM configuration_chain AS current
                    JOIN queued_input_origin AS source
                      ON source.turn_id = current.source_configuration_turn_id
                     AND source.session_id = current.session_id
              )
              SELECT model_settings_evidence_required
                FROM configuration_chain
               WHERE source_configuration_turn_id IS NULL
         ) AS configuration_origin ON TRUE
         LEFT JOIN session_defaults_version AS defaults
           ON defaults.session_id = typed.result_session_id
          AND defaults.version = COALESCE(
                queued.defaults_version,
                typed.result_selected_defaults_version
              )
         LEFT JOIN turn_model_settings_resolved AS settings
           ON settings.accepted_input_id = accepted.accepted_input_id
          AND settings.turn_id = queued.turn_id
         WHERE registry.command_id = ANY($1)",
    )
    .bind(command_ids)
    .fetch_all(&mut *connection)
    .await?;

    Ok(rows)
}

pub(super) async fn load_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
    let command_uuid = durable_command_id_to_uuid(command_id);
    let mut rows = load_complete_rows(connection, &[command_uuid]).await?;
    let Some(row) = rows.pop() else {
        return Ok(None);
    };
    if !rows.is_empty() {
        return Err(SubmitInputCorruption::Inconsistent("duplicate complete command rows").into());
    }
    let related = load_related_turn_evidence(connection, &row).await?;
    let existing_interrupt = load_existing_interrupt(connection, &row).await?;
    decode_complete(
        row,
        command_id,
        related.origin,
        related.non_accepted_predecessor,
        existing_interrupt,
    )
    .await
    .map(Some)
}

pub(super) async fn load_existing_interrupt(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<Option<AppliedInterruptCommandResult>, SubmitInputRepositoryError> {
    let Some(command_uuid) =
        row.try_get::<Option<Uuid>, _>("result_existing_interrupt_command_id")?
    else {
        return Ok(None);
    };
    let command = durable_command_id_from_uuid(command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("existing interrupt command identity"))?;
    let mut rows = load_complete_rows(connection, &[command_uuid]).await?;
    let interrupt_row = rows
        .pop()
        .ok_or(SubmitInputCorruption::Missing("existing interrupt command"))?;
    if !rows.is_empty() {
        return Err(
            SubmitInputCorruption::Inconsistent("duplicate existing interrupt command").into(),
        );
    }
    let predecessor = load_related_turn_evidence(connection, &interrupt_row).await?;
    let receipt = decode_complete(
        interrupt_row,
        command,
        predecessor.origin,
        predecessor.non_accepted_predecessor,
        None,
    )
    .await?;
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        return Err(
            SubmitInputCorruption::Inconsistent("existing interrupt was not applied").into(),
        );
    };
    origin
        .applied_interrupt()
        .copied()
        .map(Some)
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("existing interrupt authority").into())
}

pub(super) struct RelatedTurnEvidence {
    pub(super) origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    pub(super) non_accepted_predecessor: Option<NonAcceptedTurnPredecessorReconstitutionInput>,
}

async fn load_related_turn_evidence(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<RelatedTurnEvidence, SubmitInputRepositoryError> {
    let Some(key) = related_turn_origin_key(row)? else {
        if non_accepted_predecessor(row)?.is_some() {
            return Err(SubmitInputCorruption::Inconsistent(
                "non-accepted predecessor without related turn",
            )
            .into());
        }
        return Ok(RelatedTurnEvidence {
            origin: None,
            non_accepted_predecessor: None,
        });
    };
    if let Some(predecessor) = non_accepted_predecessor(row)? {
        if (
            predecessor.session.into_uuid(),
            predecessor.turn.into_uuid(),
        ) != key
        {
            return Err(SubmitInputCorruption::Inconsistent(
                "non-accepted predecessor correlation",
            )
            .into());
        }
        return Ok(RelatedTurnEvidence {
            origin: None,
            non_accepted_predecessor: Some(predecessor),
        });
    }
    let mut origins = load_turn_origin_graph(connection, &BTreeSet::from([key])).await?;
    let origin = origins
        .remove(&key)
        .ok_or(SubmitInputCorruption::Missing("related turn origin"))?;
    Ok(RelatedTurnEvidence {
        origin: Some(origin),
        non_accepted_predecessor: None,
    })
}

pub(super) fn non_accepted_predecessor(
    row: &PgRow,
) -> Result<Option<NonAcceptedTurnPredecessorReconstitutionInput>, SubmitInputRepositoryError> {
    let session: Option<Uuid> = row.try_get("non_accepted_predecessor_session_id")?;
    let turn: Option<Uuid> = row.try_get("non_accepted_predecessor_turn_id")?;
    match (session, turn) {
        (None, None) => Ok(None),
        (Some(session), Some(turn)) => Ok(Some(NonAcceptedTurnPredecessorReconstitutionInput {
            session: session_id_from_uuid(session),
            turn: turn_id_from_uuid(turn),
        })),
        _ => Err(SubmitInputCorruption::Inconsistent("non-accepted predecessor shape").into()),
    }
}

pub(super) fn related_turn_origin_key(
    row: &PgRow,
) -> Result<Option<StoredTurnOriginKey>, SubmitInputRepositoryError> {
    let result_kind: Option<String> = row.try_get("result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let delivery_kind: Option<String> = row.try_get("command_delivery_kind")?;
    let source_turn = match (
        result_kind.as_deref(),
        rejection_kind.as_deref(),
        delivery_kind.as_deref(),
    ) {
        (Some(APPLIED), None, Some("interrupt" | "after_current_turn" | "next_safe_point")) => {
            required(row, "command_expected_active_turn_id")?
        }
        (
            Some(REJECTED),
            Some(
                "active_turn_present"
                | "active_turn_mismatch"
                | "safe_point_unavailable_while_stopping"
                | "interrupt_already_applied"
                | "interrupt_unavailable_while_awaiting_approval",
            ),
            _,
        ) => required(row, "result_actual_active_turn_id")?,
        (
            Some(REJECTED),
            Some(
                "session_defaults_version_mismatch"
                | "unknown_model_alias"
                | "acceptance_position_exhausted",
            ),
            Some("interrupt" | "after_current_turn" | "next_safe_point"),
        ) => required(row, "command_expected_active_turn_id")?,
        _ => return Ok(None),
    };
    Ok(Some((required(row, "result_session_id")?, source_turn)))
}

fn decode_stored_turn_origin_provenance(
    row: &PgRow,
) -> Result<(StoredTurnOriginProvenance, Option<Uuid>), SubmitInputRepositoryError> {
    let command: Option<Uuid> = row.try_get("origin_command_id")?;
    let generation: Option<Decimal> = row.try_get("origin_goal_generation")?;
    match (command, generation) {
        // A bound dispatch turn carries both: the command accepted its tagged
        // context and the generation records the authority it runs under. The
        // command is what reconstitutes the origin, so it decides the shape and
        // the generation rides alongside it.
        (Some(command), _) => {
            let command_id = durable_command_id_from_uuid(command)
                .map_err(|_| SubmitInputCorruption::Inconsistent("turn origin command identity"))?;
            Ok((
                StoredTurnOriginProvenance::Submit(command_id),
                Some(command),
            ))
        }
        (None, Some(generation)) => {
            let generation = positive_u64_from_numeric(generation).map_err(|reason| {
                SubmitInputCorruption::InvalidOrdinal {
                    field: "origin_goal_generation",
                    reason,
                }
            })?;
            let generation = NonZeroU64::new(generation).ok_or(
                SubmitInputCorruption::Inconsistent("goal origin generation"),
            )?;
            let source_event: Option<Decimal> = row.try_get("origin_goal_source_event_ordinal")?;
            let predecessor: Option<Uuid> = row.try_get("origin_goal_predecessor_turn_id")?;
            let source = match (source_event, predecessor) {
                (Some(event), None) => {
                    let event = positive_u64_from_numeric(event).map_err(|reason| {
                        SubmitInputCorruption::InvalidOrdinal {
                            field: "origin_goal_source_event_ordinal",
                            reason,
                        }
                    })?;
                    GoalTurnSource::UserEvent(GoalEventOrdinal::new(NonZeroU64::new(event).ok_or(
                        SubmitInputCorruption::Inconsistent("goal origin source event"),
                    )?))
                }
                (None, Some(predecessor)) => {
                    GoalTurnSource::PredecessorTurn(turn_id_from_uuid(predecessor))
                }
                (Some(_), Some(_)) | (None, None) => {
                    return Err(SubmitInputCorruption::Inconsistent("goal origin source").into());
                }
            };
            let content = decode_content(
                required(row, "origin_content_parts")?,
                "goal origin content",
            )?;
            Ok((
                StoredTurnOriginProvenance::Goal {
                    generation: GoalGeneration::new(generation),
                    source,
                    content,
                },
                None,
            ))
        }
        // Neither a command nor a generation names no origin at all.
        (None, None) => Err(SubmitInputCorruption::Inconsistent("turn origin provenance").into()),
    }
}

pub(crate) async fn load_turn_origin_graph(
    connection: &mut PgConnection,
    roots: &BTreeSet<StoredTurnOriginKey>,
) -> Result<
    BTreeMap<StoredTurnOriginKey, SubmitInputTurnOriginReconstitutionInput>,
    SubmitInputRepositoryError,
> {
    if roots.is_empty() {
        return Ok(BTreeMap::new());
    }

    let source_sessions = roots
        .iter()
        .map(|(session, _)| *session)
        .collect::<Vec<_>>();
    let source_turns = roots.iter().map(|(_, turn)| *turn).collect::<Vec<_>>();
    let link_rows = sqlx::query(
        "WITH RECURSIVE origin_turn(session_id, turn_id) AS (
            SELECT root.session_id, root.turn_id
              FROM UNNEST($1::uuid[], $2::uuid[]) AS root(session_id, turn_id)
            UNION
            SELECT
                current.session_id,
                CASE accepted.disposition_kind
                    WHEN 'reclassified_as_turn_origin'
                        THEN accepted.expected_active_turn_id
                    ELSE command.expected_active_turn_id
                END
              FROM origin_turn AS current
              JOIN turn_lifecycle AS turn
                ON turn.turn_id = current.turn_id
               AND turn.session_id = current.session_id
              JOIN queued_input_origin AS queued
                ON queued.turn_id = turn.turn_id
               AND queued.session_id = turn.session_id
               AND queued.accepted_input_id = turn.origin_accepted_input_id
              JOIN accepted_input AS accepted
                ON accepted.accepted_input_id = queued.accepted_input_id
               AND accepted.session_id = turn.session_id
               AND accepted.origin_turn_id = turn.turn_id
               AND accepted.disposition_kind IN (
                    'origin_of',
                    'reclassified_as_turn_origin'
               )
              LEFT JOIN submit_input_command AS command
                ON command.command_id = accepted.accepting_command_id
             WHERE (
                    accepted.disposition_kind = 'reclassified_as_turn_origin'
                    AND accepted.expected_active_turn_id IS NOT NULL
               ) OR (
                    accepted.disposition_kind = 'origin_of'
                    AND command.delivery_kind IN (
                        'interrupt',
                        'after_current_turn'
                    )
                    AND command.expected_active_turn_id IS NOT NULL
               )
        )
        SELECT
            current.session_id AS origin_session_id,
            current.turn_id AS origin_turn_id,
            accepted.accepting_command_id AS origin_command_id,
            accepted.accepted_input_id AS origin_accepted_input_id,
            (
                SELECT COALESCE(
                    jsonb_agg(
                        jsonb_build_object(
                            'position', part.position,
                            'part_kind', part.part_kind,
                            'text_value', part.text_value,
                            'blob_digest', CASE
                                WHEN part.blob_digest IS NULL THEN NULL
                                ELSE 'sha256:' || encode(part.blob_digest, 'hex')
                            END,
                            'attachment_kind', part.attachment_kind,
                            'declared_media_type', part.declared_media_type,
                            'display_filename', part.display_filename
                        ) ORDER BY part.position
                    ),
                    '[]'::jsonb
                )
                  FROM accepted_input_content_part AS part
                 WHERE part.accepted_input_id = accepted.accepted_input_id
            ) AS origin_content_parts,
            goal.goal_generation AS origin_goal_generation,
            goal.source_event_ordinal AS origin_goal_source_event_ordinal,
            goal.predecessor_turn_id AS origin_goal_predecessor_turn_id,
            accepted.disposition_kind AS origin_disposition_kind,
            accepted.expected_active_turn_id AS reclassified_source_turn_id,
            queued.acceptance_position AS origin_acceptance_position,
            queued.priority_kind AS origin_priority_kind,
            queued.interrupt_predecessor_turn_id AS origin_interrupt_predecessor_turn_id,
            command.delivery_kind AS origin_delivery_kind,
            command.expected_active_turn_id AS origin_predecessor_turn_id,
            source.state_kind AS source_state_kind,
            source.terminal_disposition_kind AS source_terminal_disposition_kind,
            source.terminal_model_call_id AS source_terminal_model_call_id,
            source.terminal_tool_attempt_id AS source_terminal_tool_attempt_id,
            source_automatic.model_call_id
                AS source_automatic_reconciliation_model_call_id,
            source_automatic.tool_attempt_id
                AS source_automatic_reconciliation_tool_attempt_id,
            source_automatic.state_kind
                AS source_automatic_reconciliation_state_kind,
            source_automatic.attempt_count
                AS source_automatic_reconciliation_attempt_count,
            COALESCE(
                source_attempt.interrupt_command_id,
                source_interrupt.command_id
            ) AS source_interrupt_command_id
          FROM origin_turn AS current
          JOIN turn_lifecycle AS turn
            ON turn.turn_id = current.turn_id
           AND turn.session_id = current.session_id
          JOIN queued_input_origin AS queued
            ON queued.turn_id = turn.turn_id
           AND queued.session_id = turn.session_id
           AND queued.accepted_input_id = turn.origin_accepted_input_id
          JOIN accepted_input AS accepted
            ON accepted.accepted_input_id = queued.accepted_input_id
           AND accepted.session_id = turn.session_id
           AND accepted.origin_turn_id = turn.turn_id
           AND accepted.disposition_kind IN (
                'origin_of',
                'reclassified_as_turn_origin'
           )
          LEFT JOIN submit_input_command AS command
            ON command.command_id = accepted.accepting_command_id
          LEFT JOIN goal_turn AS goal
            ON goal.session_id = accepted.session_id
           AND goal.turn_id = accepted.origin_turn_id
           AND goal.accepted_input_id = accepted.accepted_input_id
          LEFT JOIN turn_lifecycle AS source
            ON source.turn_id = accepted.expected_active_turn_id
           AND source.session_id = accepted.session_id
          LEFT JOIN turn_attempt AS source_attempt
            ON source_attempt.turn_attempt_id = source.terminal_attempt_id
           AND source_attempt.turn_id = source.turn_id
           AND source_attempt.session_id = source.session_id
          LEFT JOIN automatic_reconciliation AS source_automatic
            ON source_automatic.turn_id = source.turn_id
           AND source_automatic.session_id = source.session_id
          LEFT JOIN LATERAL (
                SELECT interrupt.command_id
                  FROM submit_input_command AS interrupt
                  JOIN accepted_input AS interrupt_accepted
                    ON interrupt_accepted.accepting_command_id =
                        interrupt.command_id
                   AND interrupt_accepted.accepted_input_id =
                        interrupt.result_accepted_input_id
                   AND interrupt_accepted.session_id =
                        interrupt.result_session_id
                   AND interrupt_accepted.origin_turn_id =
                        interrupt.result_turn_id
                  JOIN queued_input_origin AS interrupt_successor
                    ON interrupt_successor.accepted_input_id =
                        interrupt_accepted.accepted_input_id
                   AND interrupt_successor.turn_id =
                        interrupt_accepted.origin_turn_id
                   AND interrupt_successor.session_id =
                        interrupt_accepted.session_id
                   AND interrupt_successor.priority_kind =
                        'interrupt_immediately_after'
                   AND interrupt_successor.interrupt_predecessor_turn_id =
                        source.turn_id
                 WHERE interrupt.session_id = source.session_id
                   AND interrupt.delivery_kind = 'interrupt'
                   AND interrupt.expected_active_turn_id = source.turn_id
                   AND interrupt.result_kind = 'applied'
                   AND interrupt.rejection_kind IS NULL
                   AND interrupt_accepted.disposition_kind = 'origin_of'
          ) AS source_interrupt ON TRUE
         ORDER BY current.session_id, current.turn_id",
    )
    .bind(&source_sessions)
    .bind(&source_turns)
    .fetch_all(&mut *connection)
    .await?;

    let mut links = BTreeMap::new();
    let mut commands = BTreeMap::new();
    for row in link_rows {
        let key = (
            required(&row, "origin_session_id")?,
            required(&row, "origin_turn_id")?,
        );
        let (provenance, command_uuid) = decode_stored_turn_origin_provenance(&row)?;
        let accepted_input =
            accepted_input_id_from_uuid(required(&row, "origin_accepted_input_id")?);
        let queue_position = decode_position(&row, "origin_acceptance_position")?;
        let interrupt_predecessor: Option<Uuid> =
            row.try_get("origin_interrupt_predecessor_turn_id")?;
        let queue_order = match required::<String>(&row, "origin_priority_kind")?.as_str() {
            "ordinary" if interrupt_predecessor.is_none() => {
                AcceptedInputQueueOrder::ordinary(queue_position)
            }
            "interrupt_immediately_after" => AcceptedInputQueueOrder::interrupt_immediately_after(
                queue_position,
                turn_id_from_uuid(interrupt_predecessor.ok_or(SubmitInputCorruption::Missing(
                    "origin_interrupt_predecessor_turn_id",
                ))?),
            ),
            "ordinary" => {
                return Err(SubmitInputCorruption::Inconsistent("ordinary origin priority").into());
            }
            value => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "origin_priority_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        let disposition_kind: String = required(&row, "origin_disposition_kind")?;
        let delivery_kind: Option<String> = row.try_get("origin_delivery_kind")?;
        let predecessor_turn: Option<Uuid> = row.try_get("origin_predecessor_turn_id")?;
        let reclassified_source: Option<Uuid> = row.try_get("reclassified_source_turn_id")?;
        let source_state: Option<String> = row.try_get("source_state_kind")?;
        let source_disposition: Option<String> = row.try_get("source_terminal_disposition_kind")?;
        let kind = match &provenance {
            StoredTurnOriginProvenance::Goal { .. } => {
                let ordinary_priority = match queue_order.priority() {
                    AcceptedInputQueuePriority::Ordinary => true,
                    AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. } => false,
                };
                if disposition_kind != "origin_of"
                    || delivery_kind.is_some()
                    || predecessor_turn.is_some()
                    || reclassified_source.is_some()
                    || !ordinary_priority
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("goal turn origin shape").into(),
                    );
                }
                StoredTurnOriginKind::Direct { predecessor: None }
            }
            StoredTurnOriginProvenance::Submit(_) => match (
                disposition_kind.as_str(),
                delivery_kind
                    .as_deref()
                    .ok_or(SubmitInputCorruption::Missing("origin_delivery_kind"))?,
                predecessor_turn,
                reclassified_source,
            ) {
                ("origin_of", "start_when_no_active_turn", None, None) => {
                    StoredTurnOriginKind::Direct { predecessor: None }
                }
                ("origin_of", "after_current_turn", Some(turn), Some(source)) if turn == source => {
                    StoredTurnOriginKind::Direct {
                        predecessor: Some((key.0, turn)),
                    }
                }
                ("origin_of", "interrupt", Some(turn), Some(source))
                    if turn == source && interrupt_predecessor == Some(turn) =>
                {
                    StoredTurnOriginKind::Direct {
                        predecessor: Some((key.0, turn)),
                    }
                }
                ("reclassified_as_turn_origin", "next_safe_point", Some(source), Some(binding))
                    if source == binding && source_state.as_deref() == Some("terminal") =>
                {
                    let source_disposition = match source_disposition.as_deref() {
                        Some("completed") => StoredTerminalTurnDisposition::Completed,
                        Some("refused") => StoredTerminalTurnDisposition::Refused,
                        Some("failed") => StoredTerminalTurnDisposition::Failed,
                        Some("cancelled") => {
                            let command = durable_command_id_from_uuid(required(
                                &row,
                                "source_interrupt_command_id",
                            )?)
                            .map_err(|_| {
                                SubmitInputCorruption::Inconsistent(
                                    "cancelled source interrupt command",
                                )
                            })?;
                            StoredTerminalTurnDisposition::Cancelled {
                                interrupt_command: command,
                            }
                        }
                        Some("reconciliation_required") => {
                            let model_call: Option<Uuid> =
                                row.try_get("source_terminal_model_call_id")?;
                            let tool_attempt: Option<Uuid> =
                                row.try_get("source_terminal_tool_attempt_id")?;
                            let ambiguous_operation = match (model_call, tool_attempt) {
                                (Some(call), None) => {
                                    IssuedOperationRef::ModelCall(ModelCallId::from_uuid(call))
                                }
                                (None, Some(attempt)) => IssuedOperationRef::ToolAttempt(
                                    ToolAttemptId::from_uuid(attempt),
                                ),
                                (Some(_), Some(_)) | (None, None) => {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "reconciliation source ambiguous operation",
                                    )
                                    .into());
                                }
                            };
                            let command: Option<Uuid> =
                                row.try_get("source_interrupt_command_id")?;
                            let automatic_call: Option<Uuid> =
                                row.try_get("source_automatic_reconciliation_model_call_id")?;
                            let automatic_tool_attempt: Option<Uuid> =
                                row.try_get("source_automatic_reconciliation_tool_attempt_id")?;
                            let automatic_state: Option<String> =
                                row.try_get("source_automatic_reconciliation_state_kind")?;
                            let automatic_attempts: Option<i32> =
                                row.try_get("source_automatic_reconciliation_attempt_count")?;
                            let authority = if let Some(command) = command {
                                StoredAutomaticReconciliationAuthority::AppliedInterrupt(
                                    durable_command_id_from_uuid(command).map_err(|_| {
                                        SubmitInputCorruption::Inconsistent(
                                            "reconciliation source interrupt command",
                                        )
                                    })?,
                                )
                            } else {
                                let (Some("reconciled"), Some(attempts)) =
                                    (automatic_state.as_deref(), automatic_attempts)
                                else {
                                    return Err(SubmitInputCorruption::Missing(
                                        "reconciliation source authority",
                                    )
                                    .into());
                                };
                                let operation_matches = match ambiguous_operation {
                                    IssuedOperationRef::ModelCall(ambiguous_call) => {
                                        automatic_call == Some(ambiguous_call.into_uuid())
                                            && automatic_tool_attempt.is_none()
                                    }
                                    IssuedOperationRef::ToolAttempt(ambiguous_attempt) => {
                                        automatic_tool_attempt
                                            == Some(ambiguous_attempt.into_uuid())
                                            && automatic_call.is_none()
                                    }
                                };
                                if !operation_matches {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "reconciliation source automatic operation",
                                    )
                                    .into());
                                }
                                let attempt = u32::try_from(attempts)
                                    .ok()
                                    .and_then(NonZeroU32::new)
                                    .filter(|attempt| attempt.get() <= 5)
                                    .ok_or(SubmitInputCorruption::Inconsistent(
                                        "reconciliation source automatic attempt",
                                    ))?;
                                StoredAutomaticReconciliationAuthority::AutomaticRecovery(attempt)
                            };
                            StoredTerminalTurnDisposition::ReconciliationRequired {
                                authority,
                                ambiguous_operation,
                            }
                        }
                        Some(value) => {
                            return Err(SubmitInputCorruption::Unsupported {
                                field: "reclassified source terminal disposition",
                                value: value.to_owned(),
                            }
                            .into());
                        }
                        None => {
                            return Err(SubmitInputCorruption::Missing(
                                "reclassified source terminal disposition",
                            )
                            .into());
                        }
                    };
                    StoredTurnOriginKind::Reclassified {
                        source: (key.0, source),
                        source_disposition,
                    }
                }
                ("origin_of" | "reclassified_as_turn_origin", _, _, _) => {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "turn origin predecessor shape",
                    )
                    .into());
                }
                (value, _, _, _) => {
                    return Err(SubmitInputCorruption::Unsupported {
                        field: "turn origin accepted-input disposition_kind",
                        value: value.to_owned(),
                    }
                    .into());
                }
            },
        };
        if links
            .insert(
                key,
                StoredTurnOriginLink {
                    provenance,
                    kind,
                    accepted_input,
                    queue_order,
                },
            )
            .is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("duplicate turn origin").into());
        }
        if let Some(command_uuid) = command_uuid
            && commands.insert(command_uuid, key).is_some()
        {
            return Err(
                SubmitInputCorruption::Inconsistent("turn origin command reused by turns").into(),
            );
        }
    }

    for root in roots {
        if !links.contains_key(root) {
            return Err(SubmitInputCorruption::Missing("related turn origin").into());
        }
    }
    for link in links.values() {
        if let Some(dependency) = link.kind.dependency()
            && !links.contains_key(&dependency)
        {
            return Err(SubmitInputCorruption::Missing("turn origin predecessor").into());
        }
    }

    let command_uuids = commands.keys().copied().collect::<Vec<_>>();
    let complete_rows = load_complete_rows(connection, &command_uuids).await?;
    let mut rows_by_command = BTreeMap::new();
    for row in complete_rows {
        let command_uuid: Uuid = required(&row, "registry_command_id")?;
        if !commands.contains_key(&command_uuid) {
            return Err(
                SubmitInputCorruption::Inconsistent("unexpected turn origin command").into(),
            );
        }
        if rows_by_command.insert(command_uuid, row).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate turn origin command rows").into(),
            );
        }
    }
    if rows_by_command.len() != commands.len() {
        return Err(SubmitInputCorruption::Missing("turn origin command").into());
    }

    let decode_order = turn_origin_dependency_order(
        links
            .iter()
            .map(|(key, link)| (*key, link.kind.dependency())),
    )
    .ok_or(SubmitInputCorruption::Inconsistent(
        "turn origin predecessor cycle",
    ))?;
    let mut decoded = BTreeMap::new();
    for ready in decode_order {
        let link = links
            .remove(&ready)
            .ok_or(SubmitInputCorruption::Missing("related turn origin"))?;
        let command_id = match link.provenance {
            StoredTurnOriginProvenance::Submit(command_id) => command_id,
            StoredTurnOriginProvenance::Goal {
                generation,
                source,
                content,
            } => {
                let direct_origin = match link.kind {
                    StoredTurnOriginKind::Direct { predecessor: None } => true,
                    StoredTurnOriginKind::Direct {
                        predecessor: Some(_),
                    }
                    | StoredTurnOriginKind::Reclassified { .. } => false,
                };
                if !direct_origin {
                    return Err(
                        SubmitInputCorruption::Inconsistent("goal origin dependency").into(),
                    );
                }
                let turn = turn_id_from_uuid(ready.1);
                let reconstructed = SubmitInputTurnOriginReconstitutionInput::from_goal(
                    GoalTurnOriginConstructionInput {
                        generation,
                        source,
                        session: session_id_from_uuid(ready.0),
                        accepted_input: link.accepted_input,
                        turn,
                        acceptance_position: link.queue_order.acceptance_position(),
                        content,
                        lifecycle: AcceptedInputLifecycle::new(
                            link.accepted_input,
                            AcceptedInputDisposition::OriginOf(turn),
                        ),
                        queue_accepted_input: link.accepted_input,
                        queue_session: session_id_from_uuid(ready.0),
                        queue_turn: turn,
                        queue_order: link.queue_order,
                    },
                );
                decoded.insert(ready, reconstructed);
                continue;
            }
        };
        let command_uuid = durable_command_id_to_uuid(command_id);
        let row = rows_by_command
            .remove(&command_uuid)
            .ok_or(SubmitInputCorruption::Missing("turn origin command"))?;
        let dependency = link
            .kind
            .dependency()
            .map(|key| {
                decoded
                    .get(&key)
                    .cloned()
                    .ok_or(SubmitInputCorruption::Missing("turn origin predecessor"))
            })
            .transpose()?;
        let receipt = decode_complete(row, command_id, dependency.clone(), None, None).await?;
        let reconstructed = match link.kind {
            StoredTurnOriginKind::Direct { .. } => {
                let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
                    receipt.result()
                else {
                    return Err(
                        SubmitInputCorruption::Inconsistent("turn origin command result").into(),
                    );
                };
                if session_id_to_uuid(applied.session()) != ready.0
                    || turn_id_to_uuid(applied.turn()) != ready.1
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("turn origin correlation").into(),
                    );
                }
                SubmitInputTurnOriginReconstitutionInput::new(
                    SubmitInputDirectTurnOriginConstructionInput {
                        receipt,
                        lifecycle: AcceptedInputLifecycle::new(
                            link.accepted_input,
                            AcceptedInputDisposition::OriginOf(turn_id_from_uuid(ready.1)),
                        ),
                        queue_accepted_input: link.accepted_input,
                        queue_session: session_id_from_uuid(ready.0),
                        queue_turn: turn_id_from_uuid(ready.1),
                        queue_order: link.queue_order,
                    },
                )
            }
            StoredTurnOriginKind::Reclassified {
                source,
                source_disposition,
            } => {
                let SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(applied)) =
                    receipt.result()
                else {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "reclassified origin command result",
                    )
                    .into());
                };
                if session_id_to_uuid(applied.session()) != ready.0
                    || applied.accepted_input() != link.accepted_input
                    || applied.binding().source_turn() != turn_id_from_uuid(source.1)
                {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "reclassified origin correlation",
                    )
                    .into());
                }
                let source_origin = dependency
                    .ok_or(SubmitInputCorruption::Missing("reclassified source origin"))?;
                let source_turn = turn_id_from_uuid(source.1);
                let source_terminal = match source_disposition {
                    StoredTerminalTurnDisposition::Completed
                    | StoredTerminalTurnDisposition::Refused
                    | StoredTerminalTurnDisposition::Failed => {
                        SubmitInputTerminalSourceReconstitutionInput::new(
                            SubmitInputTerminalSourceConstructionInput {
                                origin: source_origin.clone(),
                                turn: source_turn,
                                disposition: source_disposition.unstopped_domain().ok_or(
                                    SubmitInputCorruption::Inconsistent(
                                        "terminal source disposition",
                                    ),
                                )?,
                            },
                        )
                    }
                    StoredTerminalTurnDisposition::Cancelled { interrupt_command } => {
                        let interrupt_uuid = durable_command_id_to_uuid(interrupt_command);
                        let mut interrupt_rows =
                            load_complete_rows(connection, &[interrupt_uuid]).await?;
                        let interrupt_row = interrupt_rows.pop().ok_or(
                            SubmitInputCorruption::Missing("cancelled source interrupt command"),
                        )?;
                        if !interrupt_rows.is_empty() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "duplicate cancelled source interrupt command",
                            )
                            .into());
                        }
                        let interrupt_receipt = decode_complete(
                            interrupt_row,
                            interrupt_command,
                            Some(source_origin.clone()),
                            None,
                            None,
                        )
                        .await?;
                        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                            interrupt_origin,
                        )) = interrupt_receipt.result()
                        else {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "cancelled source interrupt result",
                            )
                            .into());
                        };
                        SubmitInputTerminalSourceReconstitutionInput::new(
                            SubmitInputTerminalSourceConstructionInput {
                                origin: source_origin.clone(),
                                turn: source_turn,
                                disposition: signalbox_domain::TurnDisposition::Cancelled {
                                    cause: interrupt_origin
                                        .applied_interrupt()
                                        .ok_or(SubmitInputCorruption::Inconsistent(
                                            "cancelled source interrupt authority",
                                        ))?
                                        .proof(),
                                },
                            },
                        )
                    }
                    StoredTerminalTurnDisposition::ReconciliationRequired {
                        authority,
                        ambiguous_operation,
                    } => match authority {
                        StoredAutomaticReconciliationAuthority::AppliedInterrupt(
                            interrupt_command,
                        ) => {
                            let interrupt_uuid = durable_command_id_to_uuid(interrupt_command);
                            let mut interrupt_rows =
                                load_complete_rows(connection, &[interrupt_uuid]).await?;
                            let interrupt_row =
                                interrupt_rows.pop().ok_or(SubmitInputCorruption::Missing(
                                    "reconciliation source interrupt command",
                                ))?;
                            if !interrupt_rows.is_empty() {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "duplicate reconciliation source interrupt command",
                                )
                                .into());
                            }
                            let interrupt_receipt = decode_complete(
                                interrupt_row,
                                interrupt_command,
                                Some(source_origin.clone()),
                                None,
                                None,
                            )
                            .await?;
                            let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                                interrupt_origin,
                            )) = interrupt_receipt.result()
                            else {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "reconciliation source interrupt result",
                                )
                                .into());
                            };
                            let interrupt = interrupt_origin
                                .applied_interrupt()
                                .ok_or(SubmitInputCorruption::Inconsistent(
                                    "reconciliation source interrupt authority",
                                ))?
                                .proof();
                            match ambiguous_operation {
                            IssuedOperationRef::ModelCall(ambiguous_call) => {
                                SubmitInputTerminalSourceReconstitutionInput::
                                    interrupted_model_call_reconciliation(
                                        SubmitInputInterruptedModelCallReconciliationConstructionInput {
                                            origin: source_origin.clone(),
                                            turn: source_turn,
                                            ambiguous_call,
                                            interrupt,
                                        },
                                    )
                            }
                            IssuedOperationRef::ToolAttempt(ambiguous_attempt) => {
                                SubmitInputTerminalSourceReconstitutionInput::
                                    interrupted_tool_reconciliation(
                                        SubmitInputInterruptedToolReconciliationConstructionInput {
                                            origin: source_origin.clone(),
                                            turn: source_turn,
                                            ambiguous_attempt,
                                            interrupt,
                                        },
                                    )
                            }
                            }
                        }
                        StoredAutomaticReconciliationAuthority::AutomaticRecovery(attempt) => {
                            SubmitInputTerminalSourceReconstitutionInput::automatic_reconciliation(
                                SubmitInputAutomaticReconciliationConstructionInput {
                                    origin: source_origin.clone(),
                                    turn: source_turn,
                                    ambiguous_operation,
                                    attempt,
                                },
                            )
                        }
                    },
                };
                SubmitInputTurnOriginReconstitutionInput::reclassified(
                    SubmitInputReclassifiedTurnOriginConstructionInput {
                        receipt,
                        lifecycle: AcceptedInputLifecycle::new(
                            link.accepted_input,
                            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                                turn: turn_id_from_uuid(ready.1),
                                reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
                            },
                        ),
                        queue_accepted_input: link.accepted_input,
                        queue_session: session_id_from_uuid(ready.0),
                        queue_turn: turn_id_from_uuid(ready.1),
                        queue_order: link.queue_order,
                        source_terminal,
                    },
                )
            }
        };
        decoded.insert(ready, reconstructed);
    }
    debug_assert!(links.is_empty());

    Ok(decoded)
}
