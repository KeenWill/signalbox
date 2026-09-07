use super::persist_terminal::encode_attachment_preparation_failure;
use super::prepared::map_tool_evidence_error;
use super::{
    ModelCallCorruption, ModelCallRepositoryError, append_terminal_call_event, encode_disposition,
    encode_provider_failure_cause, require_single, terminalize_lifecycle,
};
use crate::mapping::{
    ToolApprovalDecisionSourceStorageKind, dangerous_tool_auto_approval_to_str,
    durable_command_id_to_uuid, session_id_to_uuid, tool_approval_decision_source_to_str,
    tool_approval_posture_to_str, tool_request_id_to_uuid, turn_id_to_uuid,
};
use crate::outbox;
use crate::outbox::{InjectionOutcomeOutbox, OutboxEvent, TurnTerminalOutboxDisposition};
use rust_decimal::Decimal;
use signalbox_application::AttachmentPreparationFailure;
use signalbox_domain::{
    AcceptedInputDisposition, AmbiguousModelCallTurn, CancelledModelCallTurn,
    CancelledToolRoundModelCallTurn, CompletedModelCallTurn, DurableCommandId, FailedModelCallTurn,
    FrozenModelSelection, ModelCallId, ProviderModelCallFailureCause, ProviderReportedTokenUsage,
    ReclassifiedPendingSteeringTurn, RefusedModelCallTurn, SemanticTranscriptEntry,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef, SessionId, ToolApprovalDecision,
    ToolDecisionSource, ToolRequest, TurnId, TurnTerminalCause,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};

pub(super) async fn persist_cancelled_tool_round(
    connection: &mut PgConnection,
    cancelled: &CancelledToolRoundModelCallTurn,
    usage: ProviderReportedTokenUsage,
    retained_input_tokens: Option<u64>,
    retained_output_tokens: Option<u64>,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call_with_retained_usage(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.call(),
        usage,
        retained_input_tokens,
        retained_output_tokens,
    )
    .await?;
    persist_ended_attempt(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.attempt(),
    )
    .await?;
    persist_tool_round_authority(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.call().id(),
        "closed_by_turn_end",
        cancelled.terminal_snapshot().frontier().snapshot(),
        cancelled.assistant_entries(),
        cancelled.requests(),
    )
    .await?;
    for entry in cancelled.closed_result_entries() {
        let SemanticTranscriptEntryPayload::ToolClosed { request } = entry.payload() else {
            return Err(ModelCallCorruption::Inconsistent("closed tool-result payload").into());
        };
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 tool_result_request_id)
             VALUES ($1, $2, 'tool_closed_by_turn_end', $3)",
        )
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.identity().into_uuid())
        .bind(tool_request_id_to_uuid(*request))
        .execute(&mut *connection)
        .await?;
    }
    let cancellation = cancelled.cancellation_entry();
    if !matches!(
        cancellation.payload(),
        SemanticTranscriptEntryPayload::TurnCancelled { turn }
            if *turn == cancelled.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("cancellation entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             cancelled_turn_id)
         VALUES ($1, $2, 'turn_cancelled', $3)",
    )
    .bind(session_id_to_uuid(cancellation.source_session()))
    .bind(cancellation.identity().into_uuid())
    .bind(turn_id_to_uuid(cancelled.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, cancelled.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        cancelled.session(),
        cancelled.turn(),
        "cancelled",
        TurnTerminalCause::InterruptApplied,
        cancelled.terminal_snapshot().frontier().snapshot(),
        Some(cancelled.attempt().id()),
        Some(cancelled.call().id()),
    )
    .await?;
    append_terminal_call_event(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.call(),
    )
    .await?;
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: cancelled.session(),
            turn: cancelled.turn(),
            disposition: TurnTerminalOutboxDisposition::Cancelled {
                cancellation_entry: cancellation.identity(),
                terminal_frontier: cancelled.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn persist_tool_round_authority(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    boundary_kind: &'static str,
    boundary_frontier: signalbox_domain::ContextFrontierId,
    assistant_entries: &[SemanticTranscriptEntry],
    requests: &[ToolRequest],
) -> Result<(), ModelCallRepositoryError> {
    let response_part_count = u64::try_from(assistant_entries.len())
        .map_err(|_| ModelCallCorruption::Inconsistent("tool response part count"))?;
    let request_count = u64::try_from(requests.len())
        .map_err(|_| ModelCallCorruption::Inconsistent("tool request count"))?;
    sqlx::query(
        "INSERT INTO tool_round
            (producing_model_call_id, session_id, turn_id, boundary_kind,
             boundary_frontier_id, response_part_count, request_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(call.into_uuid())
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .bind(boundary_kind)
    .bind(boundary_frontier.into_uuid())
    .bind(Decimal::from(response_part_count))
    .bind(Decimal::from(request_count))
    .execute(&mut *connection)
    .await?;
    for request in requests {
        let arguments_kind = match request.arguments().kind() {
            signalbox_domain::ToolArgumentsKind::Json => "json",
            signalbox_domain::ToolArgumentsKind::Undecodable => "undecodable",
        };
        let approval_posture = tool_approval_posture_to_str(request.approval_posture());
        sqlx::query(
            "INSERT INTO tool_request
                (request_id, session_id, turn_id, producing_model_call_id,
                 request_ordinal, tool_name, arguments_kind, arguments_text, approval_posture)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(tool_request_id_to_uuid(request.id()))
        .bind(session_id_to_uuid(request.session()))
        .bind(turn_id_to_uuid(request.turn()))
        .bind(request.producing_call().into_uuid())
        .bind(Decimal::from(request.ordinal().as_u32()))
        .bind(request.name().as_str())
        .bind(arguments_kind)
        .bind(request.arguments().as_str())
        .bind(approval_posture)
        .execute(&mut *connection)
        .await?;
    }
    let mut response_text_start_bytes = 0_u64;
    for (response_part_ordinal, entry) in assistant_entries.iter().enumerate() {
        let response_part_ordinal = u64::try_from(response_part_ordinal)
            .map_err(|_| ModelCallCorruption::Inconsistent("tool response part ordinal"))?;
        match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         assistant_text_value, producing_model_call_id,
                         assistant_response_part_ordinal,
                         assistant_response_text_start_bytes)
                     VALUES ($1, $2, 'assistant_text', $3, $4, $5, $6)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(value.as_str())
                .bind(producing_call.into_uuid())
                .bind(Decimal::from(response_part_ordinal))
                .bind(Decimal::from(response_text_start_bytes))
                .execute(&mut *connection)
                .await?;
                response_text_start_bytes = response_text_start_bytes
                    .checked_add(u64::try_from(value.as_str().len()).map_err(|_| {
                        ModelCallCorruption::Inconsistent("tool response text byte length")
                    })?)
                    .ok_or(ModelCallCorruption::Inconsistent(
                        "tool response text byte position",
                    ))?;
            }
            SemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call,
                block,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         assistant_text_value, producing_model_call_id,
                         assistant_response_part_ordinal)
                     VALUES ($1, $2, 'provider_compaction', $3, $4, $5)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(block.as_json())
                .bind(producing_call.into_uuid())
                .bind(Decimal::from(response_part_ordinal))
                .execute(&mut *connection)
                .await?;
            }
            SemanticTranscriptEntryPayload::ProviderReasoning {
                producing_call,
                item,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         assistant_text_value, producing_model_call_id,
                         assistant_response_part_ordinal)
                     VALUES ($1, $2, 'provider_reasoning', $3, $4, $5)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(item.as_json())
                .bind(producing_call.into_uuid())
                .bind(Decimal::from(response_part_ordinal))
                .execute(&mut *connection)
                .await?;
            }
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         producing_model_call_id, assistant_tool_request_id,
                         assistant_response_part_ordinal)
                     VALUES ($1, $2, 'assistant_tool_use', $3, $4, $5)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(producing_call.into_uuid())
                .bind(tool_request_id_to_uuid(*request))
                .bind(Decimal::from(response_part_ordinal))
                .execute(&mut *connection)
                .await?;
            }
            _ => {
                return Err(
                    ModelCallCorruption::Inconsistent("tool round assistant payload").into(),
                );
            }
        }
    }
    Ok(())
}

pub(super) fn encode_tool_approval(
    decision: &ToolApprovalDecision,
) -> (&'static str, Option<&str>) {
    match decision {
        ToolApprovalDecision::Approve => ("approve", None),
        ToolApprovalDecision::Deny { reason } => (
            "deny",
            reason
                .as_ref()
                .map(signalbox_domain::ToolDenialReason::as_str),
        ),
    }
}

pub(super) fn encode_tool_decision_source(
    source: ToolDecisionSource,
) -> Result<&'static str, ModelCallRepositoryError> {
    let storage_kind = match source {
        ToolDecisionSource::UserCommand => ToolApprovalDecisionSourceStorageKind::UserCommand,
        ToolDecisionSource::PolicyAuto => ToolApprovalDecisionSourceStorageKind::PolicyAuto,
        ToolDecisionSource::SessionBlanket => ToolApprovalDecisionSourceStorageKind::SessionBlanket,
        ToolDecisionSource::RuntimeSafety => ToolApprovalDecisionSourceStorageKind::RuntimeSafety,
        ToolDecisionSource::LifecycleClosure => {
            ToolApprovalDecisionSourceStorageKind::LifecycleClosure
        }
        ToolDecisionSource::UserOverride => ToolApprovalDecisionSourceStorageKind::UserOverride,
        ToolDecisionSource::SessionOverride | ToolDecisionSource::Delegate => {
            return Err(ModelCallRepositoryError::InvalidTransition(
                "unimplemented tool-decision source cannot be stored",
            ));
        }
    };
    Ok(tool_approval_decision_source_to_str(storage_kind))
}

pub(super) async fn persist_cancelled(
    connection: &mut PgConnection,
    cancelled: &CancelledModelCallTurn,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    if let Some(call) = cancelled.call() {
        persist_ended_call(
            connection,
            cancelled.session(),
            cancelled.turn(),
            call,
            usage,
        )
        .await?;
    }
    if let Some(attempt) = cancelled.attempt() {
        persist_ended_attempt(connection, cancelled.session(), cancelled.turn(), attempt).await?;
    }
    if !cancelled.tool_result_entries().is_empty() {
        crate::tool_loop::persist_result_entry_slice(connection, cancelled.tool_result_entries())
            .await
            .map_err(map_tool_evidence_error)?;
    }
    let entry = cancelled.cancellation_entry();
    if !matches!(
        entry.payload(),
        SemanticTranscriptEntryPayload::TurnCancelled { turn } if *turn == cancelled.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("cancellation entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, cancelled_turn_id)
         VALUES ($1, $2, 'turn_cancelled', $3)",
    )
    .bind(session_id_to_uuid(entry.source_session()))
    .bind(entry.identity().into_uuid())
    .bind(turn_id_to_uuid(cancelled.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, cancelled.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        cancelled.session(),
        cancelled.turn(),
        cancelled.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        cancelled.session(),
        cancelled.turn(),
        "cancelled",
        TurnTerminalCause::InterruptApplied,
        cancelled.terminal_snapshot().frontier().snapshot(),
        cancelled
            .attempt()
            .map(signalbox_domain::EndedTurnAttempt::id),
        cancelled.call().map(signalbox_domain::EndedModelCall::id),
    )
    .await?;
    if let Some(call) = cancelled.call() {
        append_terminal_call_event(connection, cancelled.session(), cancelled.turn(), call).await?;
    }
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: cancelled.session(),
            turn: cancelled.turn(),
            disposition: TurnTerminalOutboxDisposition::Cancelled {
                cancellation_entry: entry.identity(),
                terminal_frontier: cancelled.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

pub(super) async fn persist_completed(
    connection: &mut PgConnection,
    completed: &CompletedModelCallTurn,
    usage: ProviderReportedTokenUsage,
    retained_input_tokens: Option<u64>,
    retained_output_tokens: Option<u64>,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call_with_retained_usage(
        connection,
        completed.session(),
        completed.turn(),
        completed.call(),
        usage,
        retained_input_tokens,
        retained_output_tokens,
    )
    .await?;
    persist_ended_attempt(
        connection,
        completed.session(),
        completed.turn(),
        completed.attempt(),
    )
    .await?;
    let mut response_text_start_bytes = 0_u64;
    for (response_part_ordinal, entry) in completed.assistant_entries().iter().enumerate() {
        let ordinal =
            Decimal::from(u64::try_from(response_part_ordinal).map_err(|_| {
                ModelCallCorruption::Inconsistent("completed response part ordinal")
            })?);
        match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         assistant_text_value, producing_model_call_id,
                         assistant_response_part_ordinal,
                         assistant_response_text_start_bytes)
                     VALUES ($1, $2, 'assistant_text', $3, $4, $5, $6)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(value.as_str())
                .bind(producing_call.into_uuid())
                .bind(ordinal)
                .bind(Decimal::from(response_text_start_bytes))
                .execute(&mut *connection)
                .await?;
                response_text_start_bytes = response_text_start_bytes
                    .checked_add(u64::try_from(value.as_str().len()).map_err(|_| {
                        ModelCallCorruption::Inconsistent("completed response text byte length")
                    })?)
                    .ok_or(ModelCallCorruption::Inconsistent(
                        "completed response text byte position",
                    ))?;
            }
            SemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call,
                block,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         assistant_text_value, producing_model_call_id,
                         assistant_response_part_ordinal)
                     VALUES ($1, $2, 'provider_compaction', $3, $4, $5)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(block.as_json())
                .bind(producing_call.into_uuid())
                .bind(ordinal)
                .execute(&mut *connection)
                .await?;
            }
            SemanticTranscriptEntryPayload::ProviderReasoning {
                producing_call,
                item,
            } => {
                sqlx::query(
                    "INSERT INTO semantic_transcript_entry
                        (source_session_id, semantic_entry_id, payload_kind,
                         assistant_text_value, producing_model_call_id,
                         assistant_response_part_ordinal)
                     VALUES ($1, $2, 'provider_reasoning', $3, $4, $5)",
                )
                .bind(session_id_to_uuid(entry.source_session()))
                .bind(entry.identity().into_uuid())
                .bind(item.as_json())
                .bind(producing_call.into_uuid())
                .bind(ordinal)
                .execute(&mut *connection)
                .await?;
            }
            _ => {
                return Err(
                    ModelCallCorruption::Inconsistent("completed assistant payload").into(),
                );
            }
        }
    }
    let completion = completed.completion_entry();
    if !matches!(
        completion.payload(),
        SemanticTranscriptEntryPayload::TurnCompleted { turn } if *turn == completed.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("completion entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, completed_turn_id)
         VALUES ($1, $2, 'turn_completed', $3)",
    )
    .bind(session_id_to_uuid(completion.source_session()))
    .bind(completion.identity().into_uuid())
    .bind(turn_id_to_uuid(completed.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, completed.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        completed.session(),
        completed.turn(),
        completed.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        completed.session(),
        completed.turn(),
        "completed",
        TurnTerminalCause::Completed,
        completed.terminal_snapshot().frontier().snapshot(),
        Some(completed.attempt().id()),
        Some(completed.call().id()),
    )
    .await?;
    append_terminal_call_event(
        connection,
        completed.session(),
        completed.turn(),
        completed.call(),
    )
    .await?;
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: completed.session(),
            turn: completed.turn(),
            disposition: TurnTerminalOutboxDisposition::Completed {
                call: completed.call().id(),
                completion_entry: completion.identity(),
                terminal_frontier: completed.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

pub(super) async fn persist_failed(
    connection: &mut PgConnection,
    failed: &FailedModelCallTurn,
    cause: TurnTerminalCause,
    usage: ProviderReportedTokenUsage,
    provider_failure_cause: Option<ProviderModelCallFailureCause>,
    attachment_failure: Option<AttachmentPreparationFailure>,
) -> Result<(), ModelCallRepositoryError> {
    if let Some(call) = failed.call() {
        persist_ended_call_with_provider_failure_cause(
            connection,
            failed.session(),
            failed.turn(),
            call,
            EndedCallEvidence {
                usage,
                provider_failure_cause,
                attachment_failure,
                retained_input_tokens: None,
                retained_output_tokens: None,
            },
        )
        .await?;
    } else if attachment_failure.is_some() {
        return Err(ModelCallCorruption::Inconsistent(
            "attachment-preparation failure without a model call",
        )
        .into());
    }
    persist_ended_attempt(
        connection,
        failed.session(),
        failed.turn(),
        failed.attempt(),
    )
    .await?;
    let entry = failed.failure_entry();
    if !matches!(
        entry.payload(),
        SemanticTranscriptEntryPayload::TurnFailed { turn } if *turn == failed.turn()
    ) {
        return Err(ModelCallCorruption::Inconsistent("failure entry payload").into());
    }
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', $3)",
    )
    .bind(session_id_to_uuid(entry.source_session()))
    .bind(entry.identity().into_uuid())
    .bind(turn_id_to_uuid(failed.turn()))
    .execute(&mut *connection)
    .await?;
    insert_snapshot(connection, failed.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        failed.session(),
        failed.turn(),
        failed.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        failed.session(),
        failed.turn(),
        "failed",
        cause,
        failed.terminal_snapshot().frontier().snapshot(),
        Some(failed.attempt().id()),
        failed.call().map(signalbox_domain::EndedModelCall::id),
    )
    .await?;
    if let Some(call) = failed.call() {
        append_terminal_call_event(connection, failed.session(), failed.turn(), call).await?;
    }
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: failed.session(),
            turn: failed.turn(),
            disposition: TurnTerminalOutboxDisposition::Failed {
                failure_entry: entry.identity(),
                terminal_frontier: failed.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

pub(super) async fn persist_refused(
    connection: &mut PgConnection,
    refused: &RefusedModelCallTurn,
    usage: ProviderReportedTokenUsage,
    retained_input_tokens: Option<u64>,
    retained_output_tokens: Option<u64>,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call_with_retained_usage(
        connection,
        refused.session(),
        refused.turn(),
        refused.call(),
        usage,
        retained_input_tokens,
        retained_output_tokens,
    )
    .await?;
    persist_ended_attempt(
        connection,
        refused.session(),
        refused.turn(),
        refused.attempt(),
    )
    .await?;
    for (response_part_ordinal, entry) in refused.provider_compaction_entries().iter().enumerate() {
        let SemanticTranscriptEntryPayload::ProviderCompaction {
            producing_call,
            block,
        } = entry.payload()
        else {
            return Err(
                ModelCallCorruption::Inconsistent("refused provider compaction payload").into(),
            );
        };
        let ordinal = Decimal::from(
            u64::try_from(response_part_ordinal)
                .map_err(|_| ModelCallCorruption::Inconsistent("refused response part ordinal"))?,
        );
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 assistant_text_value, producing_model_call_id,
                 assistant_response_part_ordinal)
             VALUES ($1, $2, 'provider_compaction', $3, $4, $5)",
        )
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.identity().into_uuid())
        .bind(block.as_json())
        .bind(producing_call.into_uuid())
        .bind(ordinal)
        .execute(&mut *connection)
        .await?;
    }
    insert_snapshot(connection, refused.terminal_snapshot()).await?;
    persist_reclassified_pending_steering(
        connection,
        refused.session(),
        refused.turn(),
        refused.reclassified_pending_steering(),
    )
    .await?;
    terminalize_lifecycle(
        connection,
        refused.session(),
        refused.turn(),
        "refused",
        TurnTerminalCause::ModelRefusal,
        refused.terminal_snapshot().frontier().snapshot(),
        Some(refused.attempt().id()),
        Some(refused.call().id()),
    )
    .await?;
    append_terminal_call_event(
        connection,
        refused.session(),
        refused.turn(),
        refused.call(),
    )
    .await?;
    outbox::append(
        connection,
        OutboxEvent::TurnTerminal {
            session: refused.session(),
            turn: refused.turn(),
            disposition: TurnTerminalOutboxDisposition::Refused {
                call: refused.call().id(),
                terminal_frontier: refused.terminal_snapshot().frontier().snapshot(),
            },
        },
    )
    .await?;
    Ok(())
}

/// Settles the injection receipt of one accepted input's command, when the
/// input was accepted by a command.
pub(crate) async fn settle_injection(
    connection: &mut PgConnection,
    session: SessionId,
    command: Option<Uuid>,
    outcome: InjectionOutcomeOutbox,
) -> Result<(), sqlx::Error> {
    let Some(command) = command else {
        return Ok(());
    };
    outbox::append(
        connection,
        OutboxEvent::InjectionSettled {
            session,
            command: DurableCommandId::from_uuid(command),
            outcome,
        },
    )
    .await
}

pub(crate) async fn persist_reclassified_pending_steering(
    connection: &mut PgConnection,
    session: SessionId,
    source_turn: TurnId,
    successors: &[ReclassifiedPendingSteeringTurn],
) -> Result<(), ModelCallRepositoryError> {
    if successors.is_empty() {
        return Ok(());
    }
    // An accepted-input source turn resolves to its configuration root through
    // the queue chain. A delegation-origin source turn has no
    // `queued_input_origin` row at all — it is its own configuration root, the
    // successor's configuration comes from
    // `turn_origin_exact_model_configuration` below, and a delegated turn
    // carries no resolved per-turn settings evidence to copy — so its successor
    // requires none. Exactly one arm below produces a row; a missing
    // accepted-input root stays a corruption.
    let model_settings_evidence_required: bool = sqlx::query_scalar(
        "WITH RECURSIVE configuration_chain AS (
            SELECT source.*
              FROM queued_input_origin AS source
             WHERE source.turn_id = $1
               AND source.session_id = $2
            UNION
            SELECT ancestor.*
              FROM configuration_chain AS current
              JOIN queued_input_origin AS ancestor
                ON ancestor.turn_id = current.source_configuration_turn_id
               AND ancestor.session_id = current.session_id
         )
         SELECT model_settings_evidence_required
           FROM configuration_chain
          WHERE source_configuration_turn_id IS NULL
         UNION ALL
         SELECT FALSE
           FROM turn_lifecycle AS lifecycle
          WHERE lifecycle.turn_id = $1
            AND lifecycle.session_id = $2
            AND lifecycle.origin_kind = 'delegation'",
    )
    .bind(turn_id_to_uuid(source_turn))
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ModelCallCorruption::Missing(
        "reclassified source configuration root",
    ))?;
    for successor in successors {
        let AcceptedInputDisposition::ReclassifiedAsTurnOrigin { turn, .. } =
            successor.accepted_input().disposition()
        else {
            return Err(ModelCallCorruption::Inconsistent(
                "reclassified accepted-input disposition",
            )
            .into());
        };
        if successor.session() != session
            || successor.source_turn() != source_turn
            || successor.binding().source_turn() != source_turn
            || *turn != successor.turn()
        {
            return Err(
                ModelCallCorruption::Inconsistent("reclassified successor correlation").into(),
            );
        }

        let command: Option<Option<Uuid>> = sqlx::query_scalar(
            "UPDATE accepted_input
                SET disposition_kind = 'reclassified_as_turn_origin',
                    origin_turn_id = $1
              WHERE accepted_input_id = $2
                AND session_id = $3
                AND acceptance_position = $4
                AND delivery_kind = 'next_safe_point'
                AND expected_active_turn_id = $5
                AND disposition_kind = 'pending_steering'
                AND origin_turn_id IS NULL
            RETURNING accepting_command_id",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(session))
        .bind(crate::mapping::input_position_to_numeric(
            successor.order().acceptance_position(),
        ))
        .bind(turn_id_to_uuid(source_turn))
        .fetch_optional(&mut *connection)
        .await?;
        let command = command.ok_or(ModelCallCorruption::Inconsistent(
            "pending-steering reclassification",
        ))?;
        settle_injection(
            connection,
            session,
            command,
            InjectionOutcomeOutbox::Delivered {
                turn: Some(successor.turn()),
            },
        )
        .await?;

        let (frozen_kind, frozen_direct, frozen_alias, frozen_alias_selected) =
            match successor.effective_configuration().model() {
                FrozenModelSelection::Direct(selection) => {
                    ("direct", Some(selection.into_uuid()), None, None)
                }
                FrozenModelSelection::FrozenAlias { alias, definition } => (
                    "frozen_alias",
                    None,
                    Some(alias.into_uuid()),
                    Some(definition.selected().into_uuid()),
                ),
            };
        let queue_rows = sqlx::query(
            "WITH exact_configuration AS (
                SELECT configuration.*
                  FROM turn_origin_exact_model_configuration($5, $3) AS configuration
             )
             INSERT INTO queued_input_origin
                (turn_id, accepted_input_id, session_id, acceptance_position,
                 priority_kind, source_configuration_turn_id, defaults_version,
                 requested_model_kind, requested_direct_model_selection_id,
                 requested_model_alias_id, frozen_model_kind,
                 frozen_direct_model_selection_id, frozen_model_alias_id,
                 frozen_alias_selected_direct_id, model_parameters,
                 known_provider_failure_retry, model_fallback,
                 dangerous_tool_auto_approval)
             SELECT
                $1, accepted.accepted_input_id, accepted.session_id,
                accepted.acceptance_position, 'ordinary', source.turn_id,
                CASE WHEN source.turn_id IS NULL THEN exact.defaults_version END,
                CASE WHEN source.turn_id IS NULL THEN exact.requested_model_kind END,
                CASE WHEN source.turn_id IS NULL
                     THEN exact.requested_direct_model_selection_id END,
                CASE WHEN source.turn_id IS NULL
                     THEN exact.requested_model_alias_id END,
                CASE WHEN source.turn_id IS NULL THEN exact.frozen_model_kind END,
                CASE WHEN source.turn_id IS NULL
                     THEN exact.frozen_direct_model_selection_id END,
                CASE WHEN source.turn_id IS NULL
                     THEN exact.frozen_model_alias_id END,
                CASE WHEN source.turn_id IS NULL
                     THEN exact.frozen_alias_selected_direct_id END,
                CASE WHEN source.turn_id IS NULL THEN 'provider_defaults' END,
                CASE WHEN source.turn_id IS NULL THEN 'disabled' END,
                CASE WHEN source.turn_id IS NULL THEN 'disabled' END,
                CASE WHEN source.turn_id IS NULL THEN $10 END
               FROM accepted_input AS accepted
               JOIN turn_lifecycle AS lifecycle
                 ON lifecycle.turn_id = $5
                AND lifecycle.session_id = accepted.session_id
               CROSS JOIN exact_configuration AS exact
               LEFT JOIN queued_input_origin AS source
                 ON source.turn_id = $5
                AND source.session_id = accepted.session_id
              WHERE accepted.accepted_input_id = $2
                AND accepted.session_id = $3
                AND accepted.acceptance_position = $4
                AND accepted.disposition_kind = 'reclassified_as_turn_origin'
                AND accepted.origin_turn_id = $1
                AND accepted.expected_active_turn_id = $5
                AND lifecycle.acceptance_position < accepted.acceptance_position
                AND (source.turn_id IS NOT NULL OR lifecycle.origin_kind = 'delegation')
                AND exact.frozen_model_kind = $6
                AND exact.frozen_direct_model_selection_id IS NOT DISTINCT FROM $7
                AND exact.frozen_model_alias_id IS NOT DISTINCT FROM $8
                AND exact.frozen_alias_selected_direct_id IS NOT DISTINCT FROM $9",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(session))
        .bind(crate::mapping::input_position_to_numeric(
            successor.order().acceptance_position(),
        ))
        .bind(turn_id_to_uuid(source_turn))
        .bind(frozen_kind)
        .bind(frozen_direct)
        .bind(frozen_alias)
        .bind(frozen_alias_selected)
        .bind(dangerous_tool_auto_approval_to_str(
            successor
                .effective_configuration()
                .dangerous_tool_auto_approval(),
        ))
        .execute(&mut *connection)
        .await?
        .rows_affected();
        require_single(queue_rows, "reclassified successor queue")?;

        let lifecycle_rows = sqlx::query(
            "INSERT INTO turn_lifecycle
                (turn_id, session_id, origin_accepted_input_id,
                 acceptance_position, state_kind)
             VALUES ($1, $2, $3, $4, 'queued')",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(session_id_to_uuid(session))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(crate::mapping::input_position_to_numeric(
            successor.order().acceptance_position(),
        ))
        .execute(&mut *connection)
        .await?
        .rows_affected();
        require_single(lifecycle_rows, "reclassified successor lifecycle")?;

        let settings_rows = sqlx::query(
            "INSERT INTO turn_model_settings_resolved
                (accepted_input_id, turn_id, session_id, defaults_version,
                 selected_direct_model_id, per_call_model_settings,
                 resolved_model_settings, adjusted_from_selection_id, adjustments)
             SELECT $2, $1, source.session_id, source.defaults_version,
                    source.selected_direct_model_id, source.per_call_model_settings,
                    source.resolved_model_settings, source.adjusted_from_selection_id,
                    source.adjustments
               FROM turn_model_settings_resolved AS source
              WHERE source.turn_id = $4
                AND source.session_id = $3",
        )
        .bind(turn_id_to_uuid(successor.turn()))
        .bind(successor.accepted_input().id().into_uuid())
        .bind(session_id_to_uuid(session))
        .bind(turn_id_to_uuid(source_turn))
        .execute(&mut *connection)
        .await?
        .rows_affected();
        if settings_rows > 1 {
            return Err(ModelCallRepositoryError::Corruption(
                ModelCallCorruption::Inconsistent("reclassified successor model settings"),
            ));
        }
        if settings_rows == 0 && model_settings_evidence_required {
            return Err(ModelCallCorruption::Missing("reclassified source model settings").into());
        }
        if settings_rows == 1 {
            outbox::append(
                connection,
                OutboxEvent::TurnModelSettingsResolved {
                    session,
                    accepted_input: successor.accepted_input().id(),
                },
            )
            .await?;
        }

        outbox::append(
            connection,
            OutboxEvent::InputAccepted {
                session,
                accepted_input: successor.accepted_input().id(),
                turn: successor.turn(),
                acceptance_position: successor.order().acceptance_position(),
            },
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn persist_ambiguous(
    connection: &mut PgConnection,
    ambiguous: &AmbiguousModelCallTurn,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call(
        connection,
        ambiguous.session(),
        ambiguous.turn(),
        ambiguous.call(),
        usage,
    )
    .await?;
    persist_ended_attempt(
        connection,
        ambiguous.session(),
        ambiguous.turn(),
        ambiguous.attempt(),
    )
    .await?;
    let rows = sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_model_call_recovery',
                recovery_model_call_id = $1
          WHERE turn_id = $2
            AND session_id = $3
            AND state_kind = 'active'
            AND active_phase_kind = 'running'
            AND current_attempt_id = $4",
    )
    .bind(ambiguous.call().id().into_uuid())
    .bind(turn_id_to_uuid(ambiguous.turn()))
    .bind(session_id_to_uuid(ambiguous.session()))
    .bind(ambiguous.attempt().id().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "ambiguous recovery lifecycle")?;
    append_terminal_call_event(
        connection,
        ambiguous.session(),
        ambiguous.turn(),
        ambiguous.call(),
    )
    .await?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EncodedTokenUsage {
    pub(super) input_tokens: Option<Decimal>,
    pub(super) output_tokens: Option<Decimal>,
    pub(super) cache_creation_input_tokens: Option<Decimal>,
    pub(super) cache_read_input_tokens: Option<Decimal>,
}

#[derive(Debug)]
pub(super) struct StoredModelCallObservation {
    pub(super) session: Uuid,
    pub(super) turn: Uuid,
    pub(super) attempt: Uuid,
    pub(super) target: Uuid,
    pub(super) frontier: Uuid,
    pub(super) state: String,
    pub(super) disposition: Option<String>,
    pub(super) provider_failure_cause: Option<String>,
    pub(super) usage: EncodedTokenUsage,
    pub(super) retained_input_tokens: Option<Decimal>,
    pub(super) retained_output_tokens: Option<Decimal>,
}

pub(super) fn decode_stored_model_call_observation(
    row: &PgRow,
) -> Result<StoredModelCallObservation, sqlx::Error> {
    Ok(StoredModelCallObservation {
        session: row.try_get("session_id")?,
        turn: row.try_get("turn_id")?,
        attempt: row.try_get("turn_attempt_id")?,
        target: row.try_get("resolved_provider_model_identity_id")?,
        frontier: row.try_get("context_frontier_id")?,
        state: row.try_get("state_kind")?,
        disposition: row.try_get("terminal_disposition_kind")?,
        provider_failure_cause: row.try_get("terminal_provider_failure_cause")?,
        usage: EncodedTokenUsage {
            input_tokens: row.try_get("usage_input_tokens")?,
            output_tokens: row.try_get("usage_output_tokens")?,
            cache_creation_input_tokens: row.try_get("usage_cache_creation_input_tokens")?,
            cache_read_input_tokens: row.try_get("usage_cache_read_input_tokens")?,
        },
        retained_input_tokens: row.try_get("retained_input_tokens")?,
        retained_output_tokens: row.try_get("retained_output_tokens")?,
    })
}

pub(super) async fn persist_ended_call(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: &signalbox_domain::EndedModelCall,
    usage: ProviderReportedTokenUsage,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call_with_retained_usage(connection, session, turn, call, usage, None, None).await
}

pub(super) async fn persist_ended_call_with_retained_usage(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: &signalbox_domain::EndedModelCall,
    usage: ProviderReportedTokenUsage,
    retained_input_tokens: Option<u64>,
    retained_output_tokens: Option<u64>,
) -> Result<(), ModelCallRepositoryError> {
    persist_ended_call_with_provider_failure_cause(
        connection,
        session,
        turn,
        call,
        EndedCallEvidence {
            usage,
            provider_failure_cause: None,
            attachment_failure: None,
            retained_input_tokens,
            retained_output_tokens,
        },
    )
    .await
}

#[derive(Clone, Copy)]
pub(super) struct EndedCallEvidence {
    pub(super) usage: ProviderReportedTokenUsage,
    pub(super) provider_failure_cause: Option<ProviderModelCallFailureCause>,
    pub(super) attachment_failure: Option<AttachmentPreparationFailure>,
    pub(super) retained_input_tokens: Option<u64>,
    pub(super) retained_output_tokens: Option<u64>,
}

pub(super) async fn persist_ended_call_with_provider_failure_cause(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    call: &signalbox_domain::EndedModelCall,
    evidence: EndedCallEvidence,
) -> Result<(), ModelCallRepositoryError> {
    let EndedCallEvidence {
        usage,
        provider_failure_cause,
        attachment_failure,
        retained_input_tokens,
        retained_output_tokens,
    } = evidence;
    let usage = encode_token_usage(usage);
    let attachment_failure = attachment_failure
        .map(encode_attachment_preparation_failure)
        .transpose()?;
    let rows = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = $1,
                usage_input_tokens = $2,
                usage_output_tokens = $3,
                usage_cache_creation_input_tokens = $4,
                usage_cache_read_input_tokens = $5,
                retained_input_tokens = $6,
                retained_output_tokens = $7,
                terminal_provider_failure_cause = $8,
                terminal_attachment_preparation_failure_cause = $9,
                terminal_attachment_preparation_failure_maximum_bytes = $10
          WHERE model_call_id = $11
            AND turn_id = $12
            AND session_id = $13
            AND turn_attempt_id = $14
            AND state_kind <> 'terminal'
            AND terminal_disposition_kind IS NULL",
    )
    .bind(encode_disposition(call.disposition()))
    .bind(usage.input_tokens)
    .bind(usage.output_tokens)
    .bind(usage.cache_creation_input_tokens)
    .bind(usage.cache_read_input_tokens)
    .bind(retained_input_tokens.map(Decimal::from))
    .bind(retained_output_tokens.map(Decimal::from))
    .bind(provider_failure_cause.map(encode_provider_failure_cause))
    .bind(attachment_failure.map(|(cause, _)| cause))
    .bind(attachment_failure.and_then(|(_, maximum_bytes)| maximum_bytes))
    .bind(call.id().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .bind(call.attempt().into_uuid())
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal model call")
}

pub(super) fn encode_token_usage(usage: ProviderReportedTokenUsage) -> EncodedTokenUsage {
    EncodedTokenUsage {
        input_tokens: usage.input_tokens().map(Decimal::from),
        output_tokens: usage.output_tokens().map(Decimal::from),
        cache_creation_input_tokens: usage.cache_creation_input_tokens().map(Decimal::from),
        cache_read_input_tokens: usage.cache_read_input_tokens().map(Decimal::from),
    }
}

pub(super) async fn persist_ended_attempt(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    attempt: &signalbox_domain::EndedTurnAttempt,
) -> Result<(), ModelCallRepositoryError> {
    let (variant, disposition, interrupt_command, interrupt_predecessor) =
        encode_attempt_end(attempt.end())?;
    let rows = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = $1,
                end_disposition = $2,
                interrupt_command_id = COALESCE(interrupt_command_id, $3),
                interrupt_predecessor_turn_id =
                    COALESCE(interrupt_predecessor_turn_id, $4)
          WHERE turn_attempt_id = $5
            AND turn_id = $6
            AND session_id = $7
            AND (
                (
                    state_kind IN ('prepared', 'running', 'stop_requested')
                    AND end_variant IS NULL
                    AND end_disposition IS NULL
                )
            )
            AND (
                (
                    $3::uuid IS NULL
                    AND interrupt_command_id IS NULL
                    AND interrupt_predecessor_turn_id IS NULL
                )
                OR (
                    $3::uuid IS NOT NULL
                    AND (
                        interrupt_command_id IS NULL
                        OR interrupt_command_id = $3
                    )
                    AND (
                        interrupt_predecessor_turn_id IS NULL
                        OR interrupt_predecessor_turn_id = $4
                    )
                )
            )",
    )
    .bind(variant)
    .bind(disposition)
    .bind(interrupt_command)
    .bind(interrupt_predecessor)
    .bind(attempt.id().into_uuid())
    .bind(turn_id_to_uuid(turn))
    .bind(session_id_to_uuid(session))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    require_single(rows, "terminal model-call attempt")
}

type EncodedAttemptEnd = (&'static str, &'static str, Option<Uuid>, Option<Uuid>);

fn encode_attempt_end(
    end: &signalbox_domain::AttemptEnd,
) -> Result<EncodedAttemptEnd, ModelCallRepositoryError> {
    match end {
        signalbox_domain::AttemptEnd::WithoutStop { disposition } => {
            let disposition = match disposition {
                signalbox_domain::UnstoppedAttemptDisposition::TurnCompleted => "turn_completed",
                signalbox_domain::UnstoppedAttemptDisposition::TurnRefused => "turn_refused",
                signalbox_domain::UnstoppedAttemptDisposition::YieldedToDurableWait => {
                    "yielded_to_durable_wait"
                }
                signalbox_domain::UnstoppedAttemptDisposition::KnownFailure => "known_failure",
                signalbox_domain::UnstoppedAttemptDisposition::Lost => "lost",
                signalbox_domain::UnstoppedAttemptDisposition::Ambiguous => "ambiguous",
            };
            Ok(("without_stop", disposition, None, None))
        }
        signalbox_domain::AttemptEnd::AfterCancellation { cause, disposition } => {
            let disposition = match disposition {
                signalbox_domain::CancellationStopDisposition::TurnCompleted => "turn_completed",
                signalbox_domain::CancellationStopDisposition::TurnRefused => "turn_refused",
                signalbox_domain::CancellationStopDisposition::KnownFailure => "known_failure",
                signalbox_domain::CancellationStopDisposition::Lost => "lost",
                signalbox_domain::CancellationStopDisposition::Cancelled => "cancelled",
                signalbox_domain::CancellationStopDisposition::Ambiguous => "ambiguous",
            };
            Ok((
                "after_cancellation",
                disposition,
                Some(durable_command_id_to_uuid(cause.command())),
                Some(turn_id_to_uuid(cause.predecessor())),
            ))
        }
        signalbox_domain::AttemptEnd::AfterFatalMismatch { .. } => {
            Err(ModelCallRepositoryError::InvalidTransition(
                "initial model execution cannot persist fatal-mismatch attempt history",
            ))
        }
    }
}

pub(crate) struct SnapshotAppend<I> {
    pub(crate) owning_session: SessionId,
    pub(crate) frontier: signalbox_domain::ContextFrontierId,
    pub(crate) prefix: Option<signalbox_domain::ContextFrontierId>,
    pub(crate) member_count: u64,
    pub(crate) prefix_member_count: u64,
    pub(crate) appended_entries: I,
}

pub(crate) enum SnapshotAppendError {
    FrontierInsert(sqlx::Error),
    MemberInsert(sqlx::Error),
    MemberPositionOverflow,
}

pub(crate) async fn insert_snapshot_append<I>(
    connection: &mut PgConnection,
    append: SnapshotAppend<I>,
) -> Result<(), SnapshotAppendError>
where
    I: IntoIterator<Item = SemanticTranscriptEntryRef>,
{
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id,
             prefix_context_frontier_id, member_count)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(session_id_to_uuid(append.owning_session))
    .bind(append.frontier.into_uuid())
    .bind(
        append
            .prefix
            .map(signalbox_domain::ContextFrontierId::into_uuid),
    )
    .bind(Decimal::from(append.member_count))
    .execute(&mut *connection)
    .await
    .map_err(SnapshotAppendError::FrontierInsert)?;
    for (index, entry) in append.appended_entries.into_iter().enumerate() {
        let index =
            u64::try_from(index).map_err(|_| SnapshotAppendError::MemberPositionOverflow)?;
        let position = append
            .prefix_member_count
            .checked_add(index)
            .and_then(|index| index.checked_add(1))
            .ok_or(SnapshotAppendError::MemberPositionOverflow)?;
        sqlx::query(
            "INSERT INTO context_frontier_delta
                (owning_session_id, context_frontier_id, member_position,
                 source_session_id, semantic_entry_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(session_id_to_uuid(append.owning_session))
        .bind(append.frontier.into_uuid())
        .bind(Decimal::from(position))
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.entry().into_uuid())
        .execute(&mut *connection)
        .await
        .map_err(SnapshotAppendError::MemberInsert)?;
    }
    Ok(())
}

pub(crate) async fn insert_snapshot(
    connection: &mut PgConnection,
    snapshot: &signalbox_domain::ResolvedContextFrontierSnapshot,
) -> Result<(), ModelCallRepositoryError> {
    let member_count = u64::try_from(snapshot.entry_count())
        .map_err(|_| ModelCallCorruption::Inconsistent("frontier member count"))?;
    let appended_entry_count = snapshot.appended_entries().len();
    let prefix_member_count = snapshot
        .entry_count()
        .checked_sub(appended_entry_count)
        .and_then(|count| u64::try_from(count).ok())
        .ok_or(ModelCallCorruption::Inconsistent(
            "frontier prefix member count",
        ))?;
    insert_snapshot_append(
        connection,
        SnapshotAppend {
            owning_session: snapshot.frontier().owning_session(),
            frontier: snapshot.frontier().snapshot(),
            prefix: snapshot
                .immediate_semantic_prefix()
                .map(|prefix| prefix.snapshot()),
            member_count,
            prefix_member_count,
            appended_entries: snapshot.appended_entries(),
        },
    )
    .await
    .map_err(|error| match error {
        SnapshotAppendError::FrontierInsert(error) | SnapshotAppendError::MemberInsert(error) => {
            error.into()
        }
        SnapshotAppendError::MemberPositionOverflow => {
            ModelCallCorruption::Inconsistent("frontier member position").into()
        }
    })
}
