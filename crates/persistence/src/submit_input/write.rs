use super::decode::{StoredOriginRuntimeState, decode_origin_runtime_state};
use super::encode::{
    encode_actor, encode_content_part, encode_delivery, encode_frozen_model, encode_result,
    encode_selection,
};
use super::{
    STORAGE_VERSION, SubmitInputCorruption, SubmitInputRepositoryError, decode_delivery,
    decode_position, required,
};
use crate::command_registry::SUBMIT_INPUT_KIND;
use crate::mapping::{
    accepted_input_id_from_uuid, accepted_input_id_to_uuid, dangerous_tool_auto_approval_to_str,
    defaults_version_to_numeric, durable_command_id_to_uuid, input_position_to_numeric,
    model_settings_overlay_to_json, session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid,
    turn_id_to_uuid,
};
use crate::outbox::OutboxEvent;
use crate::{model_settings_resolution, outbox};
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueuePriority,
    AcceptedInputStartingLineage, AcceptedInputTurnSchedulingRecord,
    AcceptedInputTurnSchedulingRecordState, DeliveryRequest, DurableCommandId, ModelCallId,
    PreparedSubmitInput, SessionAcceptanceTailEntryReconstitutionInput,
    SessionAcceptanceTailReconstitutionInput, SessionId, SteeringBinding,
    SteeringReclassificationReason, SubmitInputAppliedResult, SubmitInputRejectedResult,
    SubmitInputResult, UserContent,
};
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};

pub(super) async fn load_active_acceptance_tail(
    connection: &mut PgConnection,
    session: SessionId,
    turns: &[AcceptedInputTurnSchedulingRecord],
) -> Result<Option<SessionAcceptanceTailReconstitutionInput>, SubmitInputRepositoryError> {
    let Some(active) = turns.iter().find(|record| {
        matches!(
            record.state(),
            AcceptedInputTurnSchedulingRecordState::Active { .. }
        )
    }) else {
        return Ok(None);
    };

    let rows = sqlx::query(
        "SELECT
            accepted_input_id,
            session_id,
            acceptance_position,
            disposition_kind,
            origin_turn_id,
            consuming_model_call_id,
            delivery_kind,
            descendant_scope,
            expected_active_turn_id,
            expected_defaults_version,
            model_override_kind,
            replacement_model_kind,
            replacement_direct_model_selection_id,
            replacement_model_alias_id,
            model_settings_override,
            CASE
                WHEN goal_turn_is_runtime_relevant(
                    accepted.session_id, accepted.origin_turn_id
                ) THEN 'runtime_relevant'
                ELSE 'retired_goal'
            END AS origin_runtime_state
           FROM accepted_input AS accepted
          WHERE accepted.session_id = $1
            AND accepted.acceptance_position >= $2
          ORDER BY accepted.acceptance_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(input_position_to_numeric(
        active.order().acceptance_position(),
    ))
    .fetch_all(&mut *connection)
    .await?;

    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
        let entry_session = session_id_from_uuid(required(&row, "session_id")?);
        let position = decode_position(&row, "acceptance_position")?;
        let expected_active_turn: Option<Uuid> = row.try_get("expected_active_turn_id")?;
        let delivery = decode_delivery(
            required(&row, "delivery_kind")?,
            row.try_get("descendant_scope")?,
            expected_active_turn,
            row.try_get("expected_defaults_version")?,
            row.try_get("model_override_kind")?,
            row.try_get("replacement_model_kind")?,
            row.try_get("replacement_direct_model_selection_id")?,
            row.try_get("replacement_model_alias_id")?,
            required(&row, "model_settings_override")?,
            "active acceptance-tail delivery",
        )?;
        let disposition_kind: String = required(&row, "disposition_kind")?;
        let origin_turn: Option<Uuid> = row.try_get("origin_turn_id")?;
        let consuming_call: Option<Uuid> = row.try_get("consuming_model_call_id")?;
        let disposition = match (
            disposition_kind.as_str(),
            origin_turn,
            consuming_call,
            delivery,
        ) {
            ("origin_of", Some(origin), None, _) => {
                AcceptedInputDisposition::OriginOf(turn_id_from_uuid(origin))
            }
            (
                "pending_steering",
                None,
                None,
                DeliveryRequest::NextSafePoint {
                    expected_active_turn,
                },
            ) => AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(expected_active_turn),
            },
            (
                "reclassified_as_turn_origin",
                Some(origin),
                None,
                DeliveryRequest::NextSafePoint { .. },
            ) => AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: turn_id_from_uuid(origin),
                reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
            ("consumed_as_steering", None, Some(call), DeliveryRequest::NextSafePoint { .. }) => {
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: ModelCallId::from_uuid(call),
                }
            }
            ("closed_not_delivered", None, None, DeliveryRequest::NextSafePoint { .. }) => {
                AcceptedInputDisposition::ClosedNotDelivered
            }
            (
                "origin_of"
                | "pending_steering"
                | "reclassified_as_turn_origin"
                | "consumed_as_steering"
                | "closed_not_delivered",
                _,
                _,
                _,
            ) => {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active acceptance-tail disposition",
                )
                .into());
            }
            (value, _, _, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "active acceptance-tail disposition_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        let lifecycle = AcceptedInputLifecycle::new(accepted_input, disposition);
        let runtime_state: String = required(&row, "origin_runtime_state")?;
        let entry = match decode_origin_runtime_state(&runtime_state)? {
            StoredOriginRuntimeState::RetiredGoal => {
                SessionAcceptanceTailEntryReconstitutionInput::retired_goal_origin(
                    entry_session,
                    lifecycle,
                    position,
                    delivery,
                )
            }
            StoredOriginRuntimeState::RuntimeRelevant => {
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    entry_session,
                    lifecycle,
                    position,
                    delivery,
                )
            }
        };
        entries.push(entry);
    }

    let observed_last_position = entries
        .last()
        .map(SessionAcceptanceTailEntryReconstitutionInput::position)
        .ok_or(SubmitInputCorruption::Missing(
            "active acceptance-tail origin",
        ))?;
    Ok(Some(SessionAcceptanceTailReconstitutionInput::new(
        session,
        active.accepted_input().id(),
        observed_last_position,
        entries,
    )))
}

pub(super) fn decode_starting_lineage(
    kind: Option<String>,
    predecessor: Option<Uuid>,
) -> Result<AcceptedInputStartingLineage, SubmitInputRepositoryError> {
    match (kind.as_deref(), predecessor) {
        (Some("first_in_session"), None) => Ok(AcceptedInputStartingLineage::FirstInSession),
        (Some("after"), Some(predecessor)) => Ok(AcceptedInputStartingLineage::After {
            immediate_predecessor: turn_id_from_uuid(predecessor),
        }),
        (Some("first_in_session" | "after"), _) | (None, _) => {
            Err(SubmitInputCorruption::Inconsistent("starting lineage").into())
        }
        (Some(value), _) => Err(SubmitInputCorruption::Unsupported {
            field: "start_lineage_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

pub(super) async fn insert_prepared_command(
    connection: &mut PgConnection,
    prepared: &PreparedSubmitInput,
) -> Result<(), SubmitInputRepositoryError> {
    let command = prepared.command();
    let actor = encode_actor(command.actor());
    let delivery = encode_delivery(command.delivery());
    let result = encode_result(prepared.result(), command.delivery(), command.session());
    let verified_prefix = if let signalbox_domain::SubmitInputResult::Rejected(
        signalbox_domain::SubmitInputRejectedResult::AttachmentBlobNotFound { digest: missing },
    ) = prepared.result()
    {
        // Admission traverses the canonical digest order and stops at its first
        // unavailable blob; retain the checked prefix with that immutable receipt.
        Some(
            command
                .content()
                .parts()
                .iter()
                .filter_map(|part| match part {
                    signalbox_domain::UserContentPart::Attachment { digest, .. }
                        if digest < missing =>
                    {
                        Some(*digest)
                    }
                    signalbox_domain::UserContentPart::Attachment { .. }
                    | signalbox_domain::UserContentPart::Text { .. } => None,
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|digest| digest.as_bytes().to_vec())
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };

    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind, descendant_scope,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             model_settings_override, result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position,
             result_existing_interrupt_command_id,
             result_attachment_digest, result_attachment_maximum_bytes, result_attachment_verified_prefix)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
             $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23,
             $24, $25, $26, $27, $28, $29, $30, $31, $32)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(SUBMIT_INPUT_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(actor.kind)
    .bind(actor.turn)
    .bind(actor.tool_request)
    .bind(delivery.kind)
    .bind(delivery.descendant_scope)
    .bind(delivery.expected_active_turn)
    .bind(delivery.expected_defaults_version)
    .bind(delivery.model_override_kind)
    .bind(delivery.replacement.kind)
    .bind(delivery.replacement.direct)
    .bind(delivery.replacement.alias)
    .bind(&delivery.model_settings)
    .bind(result.kind)
    .bind(result.rejection_kind)
    .bind(session_id_to_uuid(result.session))
    .bind(result.accepted_input)
    .bind(result.turn)
    .bind(result.actual_active_turn)
    .bind(result.expected_active_turn)
    .bind(result.expected_defaults_version)
    .bind(result.current_defaults_version)
    .bind(result.unknown_alias)
    .bind(result.selected_defaults_version)
    .bind(result.last_position)
    .bind(result.existing_interrupt_command)
    .bind(result.attachment_digest)
    .bind(result.attachment_maximum_bytes)
    .bind(verified_prefix)
    .execute(&mut *connection)
    .await?;

    insert_command_content_parts(connection, command.command_id(), command.content()).await?;

    Ok(())
}

pub(super) async fn insert_prepared_effects(
    connection: &mut PgConnection,
    prepared: PreparedSubmitInput,
) -> Result<(), SubmitInputRepositoryError> {
    let command = prepared.command();
    let delivery = encode_delivery(command.delivery());

    if let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
        prepared.result()
    {
        let origin = applied.origin_configuration();
        let requested = encode_selection(origin.requested().model());
        let frozen = encode_frozen_model(origin.effective().model());
        let position = applied.acceptance_position();
        let (priority_kind, interrupt_predecessor) = match applied.queue_order().priority() {
            AcceptedInputQueuePriority::Ordinary => ("ordinary", None),
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor } => (
                "interrupt_immediately_after",
                Some(turn_id_to_uuid(predecessor)),
            ),
        };

        sqlx::query(
            "INSERT INTO accepted_input
                (accepted_input_id, accepting_command_id, session_id,
                 delivery_kind,
                 descendant_scope,
                 expected_active_turn_id, expected_defaults_version,
                 model_override_kind, replacement_model_kind,
                 replacement_direct_model_selection_id, replacement_model_alias_id,
                 model_settings_override, acceptance_position, disposition_kind,
                 origin_turn_id)
             VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                 $12, $13, $14, $15)",
        )
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(durable_command_id_to_uuid(command.command_id()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(delivery.kind)
        .bind(delivery.descendant_scope)
        .bind(delivery.expected_active_turn)
        .bind(delivery.expected_defaults_version)
        .bind(delivery.model_override_kind)
        .bind(delivery.replacement.kind)
        .bind(delivery.replacement.direct)
        .bind(delivery.replacement.alias)
        .bind(&delivery.model_settings)
        .bind(input_position_to_numeric(position))
        .bind("origin_of")
        .bind(turn_id_to_uuid(applied.turn()))
        .execute(&mut *connection)
        .await?;

        mirror_accepted_content_parts(connection, applied.accepted_input()).await?;

        let settings_event =
            applied
                .model_settings_event()
                .ok_or(SubmitInputCorruption::Inconsistent(
                    "resolved model settings event",
                ))?;
        model_settings_resolution::persist(connection, applied.session(), &settings_event).await?;

        sqlx::query(
            "INSERT INTO queued_input_origin
                (turn_id, accepted_input_id, session_id, acceptance_position,
                 priority_kind, defaults_version,
                 interrupt_predecessor_turn_id,
                 requested_model_kind, requested_direct_model_selection_id,
                 requested_model_alias_id, frozen_model_kind,
                 frozen_direct_model_selection_id, frozen_model_alias_id,
                 frozen_alias_selected_direct_id, model_parameters,
                 known_provider_failure_retry, model_fallback,
                 dangerous_tool_auto_approval)
             VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 $14, $15, $16, $17, $18)",
        )
        .bind(turn_id_to_uuid(applied.turn()))
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(input_position_to_numeric(position))
        .bind(priority_kind)
        .bind(defaults_version_to_numeric(
            origin.session_defaults_version(),
        ))
        .bind(interrupt_predecessor)
        .bind(requested.kind)
        .bind(requested.direct)
        .bind(requested.alias)
        .bind(frozen.kind)
        .bind(frozen.direct)
        .bind(frozen.alias)
        .bind(frozen.alias_selected)
        .bind("provider_defaults")
        .bind("disabled")
        .bind("disabled")
        .bind(dangerous_tool_auto_approval_to_str(
            origin.effective().dangerous_tool_auto_approval(),
        ))
        .execute(&mut *connection)
        .await?;

        sqlx::query(
            "INSERT INTO turn_lifecycle
                (turn_id, session_id, origin_accepted_input_id,
                 acceptance_position, state_kind)
             VALUES ($1, $2, $3, $4, 'queued')",
        )
        .bind(turn_id_to_uuid(applied.turn()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(input_position_to_numeric(position))
        .execute(&mut *connection)
        .await?;

        outbox::append(
            connection,
            OutboxEvent::InputAccepted {
                session: applied.session(),
                accepted_input: applied.accepted_input(),
                turn: applied.turn(),
                acceptance_position: position,
            },
        )
        .await?;
    }

    if let SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(applied)) =
        prepared.result()
    {
        sqlx::query(
            "INSERT INTO accepted_input
                (accepted_input_id, accepting_command_id, session_id,
                 delivery_kind,
                 descendant_scope,
                 expected_active_turn_id, expected_defaults_version,
                 model_override_kind, replacement_model_kind,
                 replacement_direct_model_selection_id, replacement_model_alias_id,
                 model_settings_override, acceptance_position, disposition_kind,
                 origin_turn_id)
             VALUES
                ($1, $2, $3, 'next_safe_point', NULL,
                 $4, NULL, NULL, NULL, NULL, NULL, $5, $6,
                 'pending_steering', NULL)",
        )
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(durable_command_id_to_uuid(command.command_id()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(turn_id_to_uuid(applied.binding().source_turn()))
        .bind(model_settings_overlay_to_json(
            signalbox_domain::ModelSettingsOverlay::inherit_all(),
        ))
        .bind(input_position_to_numeric(applied.acceptance_position()))
        .execute(&mut *connection)
        .await?;

        mirror_accepted_content_parts(connection, applied.accepted_input()).await?;
    }

    settle_injection_receipt(connection, &prepared).await
}

/// Settles the command's injection receipt. Pending steering settles at
/// its boundary; a session that does not exist has no receipt to carry.
pub(super) async fn settle_injection_receipt(
    connection: &mut PgConnection,
    prepared: &PreparedSubmitInput,
) -> Result<(), SubmitInputRepositoryError> {
    let command = prepared.command();
    let outcome = match prepared.result() {
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) => {
            outbox::InjectionOutcomeOutbox::Delivered {
                turn: Some(applied.turn()),
            }
        }
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
        | SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound { .. }) => {
            return Ok(());
        }
        SubmitInputResult::Rejected(rejected) => {
            if matches!(
                rejected,
                SubmitInputRejectedResult::AttachmentBlobNotFound { .. }
                    | SubmitInputRejectedResult::AttachmentByteBudgetExceeded { .. }
            ) && !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM session WHERE session_id = $1)",
            )
            .bind(session_id_to_uuid(command.session()))
            .fetch_one(&mut *connection)
            .await?
            {
                return Ok(());
            }
            let kind = encode_result(prepared.result(), command.delivery(), command.session())
                .rejection_kind
                .ok_or(SubmitInputCorruption::Inconsistent("rejection kind"))?;
            outbox::InjectionOutcomeOutbox::Rejected { kind }
        }
    };
    outbox::append(
        connection,
        OutboxEvent::InjectionSettled {
            session: command.session(),
            command: command.command_id(),
            outcome,
        },
    )
    .await?;
    Ok(())
}

async fn insert_command_content_parts(
    connection: &mut PgConnection,
    command: DurableCommandId,
    content: &UserContent,
) -> Result<(), SubmitInputRepositoryError> {
    for (position, part) in content.parts().iter().enumerate() {
        let encoded = encode_content_part(part);
        sqlx::query(
            "INSERT INTO submit_input_command_content_part
                (command_id, position, part_kind, text_value, blob_digest,
                 attachment_kind, declared_media_type, display_filename)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(durable_command_id_to_uuid(command))
        .bind(
            i16::try_from(position).map_err(|_| {
                SubmitInputCorruption::Inconsistent("command content part position")
            })?,
        )
        .bind(encoded.kind)
        .bind(encoded.text)
        .bind(encoded.digest)
        .bind(encoded.attachment_kind)
        .bind(encoded.media_type)
        .bind(encoded.filename)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

async fn mirror_accepted_content_parts(
    connection: &mut PgConnection,
    accepted_input: AcceptedInputId,
) -> Result<(), SubmitInputRepositoryError> {
    sqlx::query(
        "INSERT INTO accepted_input_content_part
            (accepted_input_id, position, part_kind, text_value, blob_digest,
             attachment_kind, declared_media_type, display_filename)
         SELECT accepted.accepted_input_id, part.position, part.part_kind,
                part.text_value, part.blob_digest, part.attachment_kind,
                part.declared_media_type, part.display_filename
           FROM accepted_input AS accepted
           JOIN submit_input_command_content_part AS part
             ON part.command_id = accepted.accepting_command_id
          WHERE accepted.accepted_input_id = $1
          ORDER BY part.position",
    )
    .bind(accepted_input_id_to_uuid(accepted_input))
    .execute(&mut *connection)
    .await?;
    Ok(())
}
