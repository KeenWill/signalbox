use super::transcript_types::{ProcessToolExecutionResultDisposition, ProcessTranscriptEntry};
use super::{
    ProcessReadCorruption, ProcessReadError, decode_imported_source_speaker, decode_positive,
    decode_process_tool_approval, decode_tool_result_disposition, project_imported_entry, required,
};
use crate::conversation_import_codec::decode_content;
use crate::mapping::{ToolAttemptDispositionStorageKind, session_id_from_uuid};
use crate::outbox::{
    DispatchedDelegationWaitMode, decode_delegation_outcome, decode_delegation_provenance,
    decode_delegation_reason, decode_wait_mode,
};
use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_domain::{
    AcceptedInputId, DelegationMessageId, DirectModelSelection, ImportedConversationId,
    ImportedTranscriptEntryId, ModelCallId, SemanticTranscriptEntryId, SessionId, ToolAttemptId,
    ToolRequestId, TurnId,
};
use sqlx::Row;
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;

pub(super) fn decode_transcript_entry(
    row: &PgRow,
    entry_index: u64,
) -> Result<ProcessTranscriptEntry, ProcessReadError> {
    let source_session = session_id_from_uuid(required(row, "source_session_id")?);
    let entry = SemanticTranscriptEntryId::from_uuid(required(row, "semantic_entry_id")?);
    let payload_kind: String = required(row, "payload_kind")?;
    let origin: Option<Uuid> = row.try_get("origin_accepted_input_id")?;
    let steering_source_turn: Option<Uuid> = row.try_get("steering_source_turn_id")?;
    let failed_turn: Option<Uuid> = row.try_get("failed_turn_id")?;
    let assistant_text: Option<String> = row.try_get("assistant_text_value")?;
    let producing_call: Option<Uuid> = row.try_get("producing_model_call_id")?;
    let tool_request: Option<Uuid> = row.try_get("assistant_tool_request_id")?;
    let tool_result_request: Option<Uuid> = row.try_get("tool_result_request_id")?;
    let tool_result_attempt: Option<Uuid> = row.try_get("tool_result_attempt_id")?;
    let completed_turn: Option<Uuid> = row.try_get("completed_turn_id")?;
    let cancelled_turn: Option<Uuid> = row.try_get("cancelled_turn_id")?;
    let imported_conversation: Option<Uuid> = row.try_get("imported_conversation_id")?;
    let imported_entry: Option<Uuid> = row.try_get("imported_transcript_entry_id")?;
    let model_identity_turn: Option<Uuid> = row.try_get("model_identity_turn_id")?;
    let model_identity_defaults_version: Option<Decimal> =
        row.try_get("model_identity_defaults_version")?;
    let model_identity_direct_selection: Option<Uuid> =
        row.try_get("model_identity_direct_selection_id")?;
    let context_summary_value: Option<String> = row.try_get("context_summary_value")?;
    let context_summary_call: Option<Uuid> = row.try_get("context_summary_producing_call_id")?;
    let context_summary_first_source_session: Option<Uuid> =
        row.try_get("context_summary_first_source_session_id")?;
    let context_summary_first_entry: Option<Uuid> =
        row.try_get("context_summary_first_entry_id")?;
    let context_summary_through_source_session: Option<Uuid> =
        row.try_get("context_summary_through_source_session_id")?;
    let context_summary_through_entry: Option<Uuid> =
        row.try_get("context_summary_through_entry_id")?;
    let imported_source_speaker: Option<String> = row.try_get("imported_source_speaker_kind")?;
    let imported_content: Option<Vec<u8>> = row.try_get("imported_content_encoding")?;
    let origin_content: Option<Value> = row.try_get("origin_content")?;
    let origin_turn: Option<Uuid> = row.try_get("origin_turn_id")?;
    let assistant_turn: Option<Uuid> = row.try_get("assistant_turn_id")?;
    let result_attempt_request: Option<Uuid> = row.try_get("result_attempt_request_id")?;
    let transcript_tool_name: Option<String> = row.try_get("transcript_tool_name")?;
    let transcript_tool_arguments: Option<String> = row.try_get("transcript_tool_arguments")?;
    let result_disposition: Option<String> = row.try_get("result_disposition")?;
    let result_text: Option<String> = row.try_get("result_text")?;
    let result_error_kind: Option<String> = row.try_get("result_error_kind")?;
    let result_error_detail: Option<String> = row.try_get("result_error_detail")?;
    let transcript_decision_kind: Option<String> = row.try_get("transcript_decision_kind")?;
    let transcript_denial_reason: Option<String> = row.try_get("transcript_denial_reason")?;
    let delegated_task_spawning_request: Option<Uuid> =
        row.try_get("delegated_task_spawning_tool_request_id")?;
    let delegation_message: Option<Uuid> = row.try_get("delegation_message_id")?;
    let delegation_result_awaiting_request: Option<Uuid> =
        row.try_get("delegation_result_awaiting_tool_request_id")?;
    let delegation_result_spawning_request: Option<Uuid> =
        row.try_get("delegation_result_spawning_tool_request_id")?;
    let delegated_task_content: Option<String> = row.try_get("delegated_task_content")?;
    let delegated_task_parent_session: Option<Uuid> =
        row.try_get("delegated_task_parent_session_id")?;
    let delegated_task_parent_turn: Option<Uuid> = row.try_get("delegated_task_parent_turn_id")?;
    let delegation_message_spawning_request: Option<Uuid> =
        row.try_get("delegation_message_spawning_request_id")?;
    let delegation_message_ordinal: Option<Decimal> = row.try_get("delegation_message_ordinal")?;
    let delegation_message_content: Option<String> = row.try_get("delegation_message_content")?;
    let delegation_message_sender: Option<Uuid> =
        row.try_get("delegation_message_sender_session_id")?;
    let delegation_message_recipient: Option<Uuid> =
        row.try_get("delegation_message_recipient_session_id")?;
    let delegation_message_delivery_sequence: Option<Decimal> =
        row.try_get("delegation_message_delivery_sequence")?;
    let delegation_result_child: Option<Uuid> =
        row.try_get("delegation_result_child_session_id")?;
    let delegation_result_wait_mode: Option<String> = row.try_get("delegation_result_wait_mode")?;
    let delegation_result_delivery_sequence: Option<Decimal> =
        row.try_get("delegation_result_delivery_sequence")?;
    let delegation_result_outcome: Option<String> =
        row.try_get("delegation_result_outcome_kind")?;
    let delegation_result_content: Option<String> = row.try_get("delegation_result_content")?;
    let delegation_result_reason: Option<String> = row.try_get("delegation_result_reason_kind")?;

    let legacy_payload_present = origin.is_some()
        || steering_source_turn.is_some()
        || failed_turn.is_some()
        || assistant_text.is_some()
        || producing_call.is_some()
        || tool_request.is_some()
        || tool_result_attempt.is_some()
        || completed_turn.is_some()
        || cancelled_turn.is_some()
        || imported_conversation.is_some()
        || imported_entry.is_some()
        || model_identity_turn.is_some()
        || model_identity_defaults_version.is_some()
        || model_identity_direct_selection.is_some()
        || context_summary_value.is_some()
        || context_summary_call.is_some()
        || context_summary_first_source_session.is_some()
        || context_summary_first_entry.is_some()
        || context_summary_through_source_session.is_some()
        || context_summary_through_entry.is_some();

    if payload_kind == "delegated_task" {
        let (Some(spawning_request), Some(parent_session), Some(parent_turn), Some(content)) = (
            delegated_task_spawning_request,
            delegated_task_parent_session,
            delegated_task_parent_turn,
            delegated_task_content,
        ) else {
            return Err(ProcessReadCorruption::Inconsistent("delegated-task entry shape").into());
        };
        if legacy_payload_present
            || tool_result_request.is_some()
            || delegation_message.is_some()
            || delegation_result_awaiting_request.is_some()
            || delegation_result_spawning_request.is_some()
            || content.is_empty()
        {
            return Err(ProcessReadCorruption::Inconsistent("delegated-task entry shape").into());
        }
        return Ok(ProcessTranscriptEntry::DelegatedTask {
            entry_index,
            source_session,
            entry,
            spawning_request: ToolRequestId::from_uuid(spawning_request),
            parent_session: SessionId::from_uuid(parent_session),
            parent_turn: TurnId::from_uuid(parent_turn),
            content,
        });
    }

    if payload_kind == "delegation_message" {
        let (
            Some(message),
            Some(spawning_request),
            Some(sender),
            Some(recipient),
            Some(ordinal),
            Some(delivery_sequence),
            Some(content),
        ) = (
            delegation_message,
            delegation_message_spawning_request,
            delegation_message_sender,
            delegation_message_recipient,
            delegation_message_ordinal,
            delegation_message_delivery_sequence,
            delegation_message_content,
        )
        else {
            return Err(
                ProcessReadCorruption::Inconsistent("delegation-message entry shape").into(),
            );
        };
        if legacy_payload_present
            || tool_result_request.is_some()
            || delegated_task_spawning_request.is_some()
            || delegation_result_awaiting_request.is_some()
            || delegation_result_spawning_request.is_some()
            || recipient != source_session.into_uuid()
            || content.is_empty()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("delegation-message entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::DelegationMessage {
            entry_index,
            source_session,
            entry,
            spawning_request: ToolRequestId::from_uuid(spawning_request),
            message: DelegationMessageId::from_uuid(message),
            sender: SessionId::from_uuid(sender),
            recipient: SessionId::from_uuid(recipient),
            ordinal: decode_positive(ordinal, "delegation message ordinal")?,
            delivery_sequence: decode_positive(
                delivery_sequence,
                "delegation message delivery sequence",
            )?,
            content,
        });
    }

    if payload_kind == "delegation_result" {
        let (
            Some(awaiting_request),
            Some(spawning_request),
            Some(child),
            Some(wait_mode),
            Some(outcome),
            Some(reason),
        ) = (
            delegation_result_awaiting_request,
            delegation_result_spawning_request,
            delegation_result_child,
            delegation_result_wait_mode.as_deref(),
            delegation_result_outcome.as_deref(),
            delegation_result_reason.as_deref(),
        )
        else {
            return Err(
                ProcessReadCorruption::Inconsistent("delegation-result entry shape").into(),
            );
        };
        let mode = decode_wait_mode(wait_mode)
            .map_err(|_| ProcessReadCorruption::Inconsistent("delegation-result wait mode"))?;
        let delivery_sequence = delegation_result_delivery_sequence
            .map(|value| decode_positive(value, "delegation result delivery sequence"))
            .transpose()?;
        let foreground_correlation = tool_result_request == Some(awaiting_request);
        if legacy_payload_present
            || delegated_task_spawning_request.is_some()
            || delegation_message.is_some()
            || (mode == DispatchedDelegationWaitMode::Foreground
                && (!foreground_correlation || delivery_sequence.is_some()))
            || (mode == DispatchedDelegationWaitMode::Background
                && (tool_result_request.is_some() || delivery_sequence.is_none()))
        {
            return Err(
                ProcessReadCorruption::Inconsistent("delegation-result entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::DelegationResult {
            entry_index,
            source_session,
            entry,
            awaiting_request: ToolRequestId::from_uuid(awaiting_request),
            spawning_request: ToolRequestId::from_uuid(spawning_request),
            child: SessionId::from_uuid(child),
            mode,
            delivery_sequence,
            outcome: decode_delegation_outcome(outcome)
                .map_err(|_| ProcessReadCorruption::Inconsistent("delegation-result outcome"))?,
            content: delegation_result_content,
            reason: decode_delegation_reason(reason)
                .map_err(|_| ProcessReadCorruption::Inconsistent("delegation-result reason"))?,
            provenance: decode_delegation_provenance(row)
                .map_err(|_| ProcessReadCorruption::Inconsistent("delegation-result provenance"))?,
        });
    }

    if delegated_task_spawning_request.is_some()
        || delegation_message.is_some()
        || delegation_result_awaiting_request.is_some()
        || delegation_result_spawning_request.is_some()
    {
        return Err(
            ProcessReadCorruption::Inconsistent("non-delegation semantic entry fields").into(),
        );
    }

    let transcript_approval = decode_process_tool_approval(row)?;

    if payload_kind == "context_summary" {
        let (
            Some(content),
            Some(call),
            Some(first_source_session),
            Some(first_entry),
            Some(through_source_session),
            Some(through_entry),
        ) = (
            context_summary_value,
            context_summary_call,
            context_summary_first_source_session,
            context_summary_first_entry,
            context_summary_through_source_session,
            context_summary_through_entry,
        )
        else {
            return Err(ProcessReadCorruption::Inconsistent("context-summary entry shape").into());
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_request.is_some()
            || tool_result_attempt.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || imported_conversation.is_some()
            || imported_entry.is_some()
            || model_identity_turn.is_some()
            || model_identity_defaults_version.is_some()
            || model_identity_direct_selection.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("context-summary entry shape").into());
        }
        return Ok(ProcessTranscriptEntry::ContextSummary {
            entry_index,
            source_session,
            entry,
            model_call: ModelCallId::from_uuid(call),
            first: signalbox_domain::SemanticTranscriptEntryRef::from_source(
                session_id_from_uuid(first_source_session),
                SemanticTranscriptEntryId::from_uuid(first_entry),
            ),
            through: signalbox_domain::SemanticTranscriptEntryRef::from_source(
                session_id_from_uuid(through_source_session),
                SemanticTranscriptEntryId::from_uuid(through_entry),
            ),
            content,
        });
    }
    if context_summary_value.is_some()
        || context_summary_call.is_some()
        || context_summary_first_source_session.is_some()
        || context_summary_first_entry.is_some()
        || context_summary_through_source_session.is_some()
        || context_summary_through_entry.is_some()
    {
        return Err(
            ProcessReadCorruption::Inconsistent("non-summary context-summary fields").into(),
        );
    }

    if payload_kind == "model_identity_changed" {
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_request.is_some()
            || tool_result_attempt.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || imported_conversation.is_some()
            || imported_entry.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("model identity semantic entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::ModelIdentityChanged {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(
                model_identity_turn.ok_or(ProcessReadCorruption::Missing("model identity turn"))?,
            ),
            defaults_version: decode_positive(
                model_identity_defaults_version.ok_or(ProcessReadCorruption::Missing(
                    "model identity defaults version",
                ))?,
                "model identity defaults version",
            )?,
            selected: DirectModelSelection::from_uuid(model_identity_direct_selection.ok_or(
                ProcessReadCorruption::Missing("model identity direct selection"),
            )?),
        });
    }
    if model_identity_turn.is_some()
        || model_identity_defaults_version.is_some()
        || model_identity_direct_selection.is_some()
    {
        return Err(
            ProcessReadCorruption::Inconsistent("native semantic model identity fields").into(),
        );
    }

    if payload_kind == "assistant_tool_use" {
        let (Some(call), Some(request), Some(turn), Some(name), Some(arguments)) = (
            producing_call,
            tool_request,
            assistant_turn,
            transcript_tool_name,
            transcript_tool_arguments,
        ) else {
            return Err(
                ProcessReadCorruption::Inconsistent("assistant tool-use entry shape").into(),
            );
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || tool_result_request.is_some()
            || tool_result_attempt.is_some()
            || result_attempt_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("assistant tool-use entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::AssistantToolUse {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
            model_call: ModelCallId::from_uuid(call),
            request: ToolRequestId::from_uuid(request),
            name,
            arguments,
            approval: transcript_approval,
        });
    }

    if payload_kind == "tool_execution_result" {
        let (Some(attempt), Some(request), Some(disposition)) = (
            tool_result_attempt,
            result_attempt_request,
            result_disposition.as_deref(),
        ) else {
            return Err(
                ProcessReadCorruption::Inconsistent("tool execution-result entry shape").into(),
            );
        };
        let disposition = decode_tool_result_disposition(disposition)?;
        let (disposition, content) = match (
            disposition,
            result_text,
            result_error_kind,
            result_error_detail,
        ) {
            (ToolAttemptDispositionStorageKind::Completed, Some(text), None, None) => {
                (ProcessToolExecutionResultDisposition::Completed, text)
            }
            (ToolAttemptDispositionStorageKind::KnownFailed, None, Some(kind), detail) => (
                ProcessToolExecutionResultDisposition::KnownFailed,
                serde_json::json!({
                    "error": {
                        "kind": kind,
                        "detail": detail,
                    }
                })
                .to_string(),
            ),
            (ToolAttemptDispositionStorageKind::Completed, _, _, _)
            | (ToolAttemptDispositionStorageKind::KnownFailed, _, _, _)
            | (ToolAttemptDispositionStorageKind::AwaitingChild, _, _, _)
            | (ToolAttemptDispositionStorageKind::Ambiguous, _, _, _) => {
                return Err(
                    ProcessReadCorruption::Inconsistent("tool execution-result evidence").into(),
                );
            }
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
            || assistant_turn.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("tool execution-result entry shape").into(),
            );
        }
        return Ok(ProcessTranscriptEntry::ToolExecutionResult {
            entry_index,
            source_session,
            entry,
            request: ToolRequestId::from_uuid(request),
            attempt: ToolAttemptId::from_uuid(attempt),
            disposition,
            content,
        });
    }

    if matches!(
        payload_kind.as_str(),
        "tool_denied" | "tool_closed_by_turn_end"
    ) {
        let Some(request) = tool_result_request else {
            return Err(ProcessReadCorruption::Inconsistent("tool result entry shape").into());
        };
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || tool_result_attempt.is_some()
            || result_attempt_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
            || assistant_turn.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent("tool result entry shape").into());
        }
        return Ok(if payload_kind == "tool_denied" {
            if transcript_decision_kind.as_deref() != Some("deny") {
                return Err(ProcessReadCorruption::Inconsistent("tool denial decision").into());
            }
            ProcessTranscriptEntry::ToolDenied {
                entry_index,
                source_session,
                entry,
                request: ToolRequestId::from_uuid(request),
                content: serde_json::json!({
                    "error": {
                        "kind": "denied",
                        "detail": transcript_denial_reason,
                    }
                })
                .to_string(),
            }
        } else {
            ProcessTranscriptEntry::ToolClosed {
                entry_index,
                source_session,
                entry,
                request: ToolRequestId::from_uuid(request),
                content: String::from(r#"{"error":{"detail":null,"kind":"closed_by_turn_end"}}"#),
            }
        });
    }

    if tool_result_request.is_some()
        || tool_result_attempt.is_some()
        || result_attempt_request.is_some()
    {
        return Err(ProcessReadCorruption::Inconsistent("semantic transcript tool fields").into());
    }

    if payload_kind == "imported_entry" {
        if origin.is_some()
            || steering_source_turn.is_some()
            || failed_turn.is_some()
            || assistant_text.is_some()
            || producing_call.is_some()
            || tool_request.is_some()
            || completed_turn.is_some()
            || cancelled_turn.is_some()
            || origin_content.is_some()
            || origin_turn.is_some()
            || assistant_turn.is_some()
        {
            return Err(
                ProcessReadCorruption::Inconsistent("imported semantic entry shape").into(),
            );
        }
        let imported_conversation =
            ImportedConversationId::from_uuid(imported_conversation.ok_or(
                ProcessReadCorruption::Missing("imported conversation identity"),
            )?);
        let imported_entry = ImportedTranscriptEntryId::from_uuid(
            imported_entry.ok_or(ProcessReadCorruption::Missing("imported entry identity"))?,
        );
        let source_speaker = decode_imported_source_speaker(
            imported_source_speaker
                .ok_or(ProcessReadCorruption::Missing("imported source speaker"))?,
        )?;
        let content = decode_content(
            imported_content
                .as_deref()
                .ok_or(ProcessReadCorruption::Missing("imported content encoding"))?,
        )
        .map_err(|_| ProcessReadCorruption::Inconsistent("imported content encoding"))?;
        return Ok(project_imported_entry(
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content,
        ));
    }

    if imported_conversation.is_some()
        || imported_entry.is_some()
        || imported_source_speaker.is_some()
        || imported_content.is_some()
    {
        return Err(ProcessReadCorruption::Inconsistent("native semantic entry shape").into());
    }

    let projected = match (
        payload_kind.as_str(),
        origin,
        steering_source_turn,
        failed_turn,
        assistant_text,
        producing_call,
        tool_request,
        completed_turn,
        cancelled_turn,
        origin_content,
        origin_turn,
        assistant_turn,
    ) {
        (
            "origin_accepted_input",
            Some(accepted_input),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(content),
            Some(turn),
            None,
        ) => ProcessTranscriptEntry::User {
            entry_index,
            source_session,
            entry,
            accepted_input: AcceptedInputId::from_uuid(accepted_input),
            turn: TurnId::from_uuid(turn),
            content: crate::user_content::decode(content).map_err(|_| {
                ProcessReadCorruption::Inconsistent("semantic accepted-input content")
            })?,
        },
        (
            "steering_accepted_input",
            Some(accepted_input),
            Some(turn),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(content),
            None,
            None,
        ) => ProcessTranscriptEntry::User {
            entry_index,
            source_session,
            entry,
            accepted_input: AcceptedInputId::from_uuid(accepted_input),
            turn: TurnId::from_uuid(turn),
            content: crate::user_content::decode(content).map_err(|_| {
                ProcessReadCorruption::Inconsistent("semantic accepted-input content")
            })?,
        },
        (
            "assistant_text",
            None,
            None,
            None,
            Some(content),
            Some(call),
            None,
            None,
            None,
            None,
            None,
            Some(turn),
        ) if !content.is_empty() => ProcessTranscriptEntry::Assistant {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
            model_call: ModelCallId::from_uuid(call),
            content,
        },
        (
            "provider_compaction",
            None,
            None,
            None,
            Some(block_json),
            Some(call),
            None,
            None,
            None,
            None,
            None,
            Some(turn),
        ) if signalbox_domain::ProviderCompactionBlock::try_new(block_json.clone()).is_ok() => {
            ProcessTranscriptEntry::ProviderCompaction {
                entry_index,
                source_session,
                entry,
                turn: TurnId::from_uuid(turn),
                model_call: ModelCallId::from_uuid(call),
            }
        }
        ("turn_failed", None, None, Some(turn), None, None, None, None, None, None, None, None) => {
            ProcessTranscriptEntry::TurnFailed {
                entry_index,
                source_session,
                entry,
                turn: TurnId::from_uuid(turn),
            }
        }
        (
            "turn_completed",
            None,
            None,
            None,
            None,
            None,
            None,
            Some(turn),
            None,
            None,
            None,
            None,
        ) => ProcessTranscriptEntry::TurnCompleted {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
        },
        (
            "turn_cancelled",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(turn),
            None,
            None,
            None,
        ) => ProcessTranscriptEntry::TurnCancelled {
            entry_index,
            source_session,
            entry,
            turn: TurnId::from_uuid(turn),
        },
        (
            "origin_accepted_input"
            | "steering_accepted_input"
            | "assistant_text"
            | "provider_compaction"
            | "assistant_tool_use"
            | "tool_execution_result"
            | "tool_denied"
            | "tool_closed_by_turn_end"
            | "turn_failed"
            | "turn_completed"
            | "turn_cancelled",
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
            _,
        ) => {
            return Err(
                ProcessReadCorruption::Inconsistent("semantic transcript entry shape").into(),
            );
        }
        _ => {
            return Err(ProcessReadCorruption::Unsupported {
                field: "semantic transcript payload kind",
                value: payload_kind,
            }
            .into());
        }
    };
    Ok(projected)
}
