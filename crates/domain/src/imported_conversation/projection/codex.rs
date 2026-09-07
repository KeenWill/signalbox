//! Codex rollout record projection for `docs/spec/conversation-import.md`.

use super::super::content::ImportedMediaSource;
use super::super::content::ImportedMessageContentAbsence;
use super::super::content::ImportedSourceMetadata;
use super::super::content::ImportedSpeaker;
use super::super::content::ImportedToolResultBlock;
use super::super::content::ImportedToolResultValue;
use super::super::content::ImportedTranscriptContent;
use super::super::structured_field::projected_string_structured_attestation;
use super::super::structured_field::projected_structured_attestation;
use super::super::structured_field::projected_text_attestation;
use super::super::structured_field::unique_structured_field;
use super::super::structured_value::ImportedSourceAttestation;
use super::super::structured_value::ImportedStructuredObjectMember;
use super::super::structured_value::ImportedStructuredValue;
use super::super::structured_value::ImportedText;
use super::ProjectedEntry;

pub(super) fn project_codex_record(
    normalized: &ImportedStructuredValue,
) -> Result<Vec<ProjectedEntry>, ()> {
    let ImportedStructuredValue::Object(record) = normalized else {
        return Err(());
    };
    let source_type = projected_text_attestation(record, "type")?;
    if !matches!(
        &source_type,
        ImportedSourceAttestation::Attested(value) if value.as_str() == "response_item"
    ) {
        return Ok(vec![projected_codex_source_event(
            record,
            projected_codex_optional_object(record, "payload")?,
            source_type,
            ImportedSourceAttestation::NotAttested,
        )?]);
    }
    let payload = match unique_structured_field(record, "payload")? {
        Some(ImportedStructuredValue::Object(payload)) => payload,
        None | Some(_) => return Err(()),
    };
    let payload_type = projected_text_attestation(payload, "type")?;
    match &payload_type {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "message" => {
            project_codex_message(record, payload, source_type)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "reasoning" => {
            project_codex_reasoning(record, payload, source_type)
        }
        ImportedSourceAttestation::Attested(value)
            if matches!(value.as_str(), "function_call" | "custom_tool_call") =>
        {
            project_codex_named_tool_call(record, payload)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "tool_search_call" => {
            project_codex_structured_tool_call(record, payload, "arguments")
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "local_shell_call" => {
            project_codex_structured_tool_call(record, payload, "action")
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "web_search_call" => {
            project_codex_web_search_call(record, payload)
        }
        ImportedSourceAttestation::Attested(value)
            if matches!(
                value.as_str(),
                "function_call_output" | "custom_tool_call_output"
            ) =>
        {
            project_codex_tool_result(record, payload)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "tool_search_output" => {
            project_codex_tool_search_result(record, payload)
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => Ok(vec![projected_codex_source_event(
            record,
            Some(payload),
            source_type,
            ImportedSourceAttestation::NotAttested,
        )?]),
    }
}

fn project_codex_message(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    source_type: ImportedSourceAttestation<ImportedText>,
) -> Result<Vec<ProjectedEntry>, ()> {
    let role = projected_text_attestation(payload, "role")?;
    let speaker = match &role {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "user" => {
            Some(ImportedSpeaker::User)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "assistant" => {
            Some(ImportedSpeaker::Assistant)
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => None,
    };
    let Some(speaker) = speaker else {
        return Ok(vec![projected_codex_source_event(
            record,
            Some(payload),
            source_type,
            projected_codex_role_for_metadata(&role),
        )?]);
    };
    let content = match unique_structured_field(payload, "content")? {
        None => vec![ImportedTranscriptContent::MessageContentAbsent(
            ImportedMessageContentAbsence::ContentNotAttested,
        )],
        Some(ImportedStructuredValue::Null) => {
            vec![ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::ContentAttestedAbsent,
            )]
        }
        Some(ImportedStructuredValue::String(value)) => vec![ImportedTranscriptContent::Text(
            ImportedSourceAttestation::Attested(value.clone()),
        )],
        Some(ImportedStructuredValue::Array(blocks)) if blocks.is_empty() => {
            vec![ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::EmptyBlockArray,
            )]
        }
        Some(ImportedStructuredValue::Array(blocks)) => blocks
            .iter()
            .map(projected_codex_message_block)
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(()),
    };
    let source = projected_codex_source_metadata(
        record,
        Some(payload),
        ImportedSourceAttestation::Attested(speaker),
    )?;
    Ok(content
        .into_iter()
        .map(|content| ProjectedEntry {
            source_speaker: ImportedSourceAttestation::Attested(speaker),
            content,
            source: source.clone(),
        })
        .collect())
}

fn projected_codex_role_for_metadata(
    role: &ImportedSourceAttestation<ImportedText>,
) -> ImportedSourceAttestation<ImportedSpeaker> {
    match role {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "user" => {
            ImportedSourceAttestation::Attested(ImportedSpeaker::User)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "assistant" => {
            ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant)
        }
        ImportedSourceAttestation::Attested(_) => ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::AttestedAbsent => ImportedSourceAttestation::AttestedAbsent,
        ImportedSourceAttestation::NotAttested => ImportedSourceAttestation::NotAttested,
    }
}

fn projected_codex_message_block(
    value: &ImportedStructuredValue,
) -> Result<ImportedTranscriptContent, ()> {
    let ImportedStructuredValue::Object(members) = value else {
        return Err(());
    };
    let source_type = projected_text_attestation(members, "type")?;
    match &source_type {
        ImportedSourceAttestation::Attested(value)
            if matches!(value.as_str(), "input_text" | "output_text") =>
        {
            Ok(ImportedTranscriptContent::Text(projected_text_attestation(
                members, "text",
            )?))
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => {
            Ok(ImportedTranscriptContent::SourceMessageBlock { source_type })
        }
    }
}

fn project_codex_reasoning(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    source_type: ImportedSourceAttestation<ImportedText>,
) -> Result<Vec<ProjectedEntry>, ()> {
    let mut content = Vec::new();
    projected_codex_reasoning_blocks(payload, "summary", &mut content)?;
    projected_codex_reasoning_blocks(payload, "content", &mut content)?;
    match unique_structured_field(payload, "encrypted_content")? {
        None => {}
        Some(ImportedStructuredValue::Null) => {
            content.push(ImportedTranscriptContent::RedactedThinking {
                data: ImportedSourceAttestation::AttestedAbsent,
            });
        }
        Some(ImportedStructuredValue::String(value)) => {
            content.push(ImportedTranscriptContent::RedactedThinking {
                data: ImportedSourceAttestation::Attested(value.clone()),
            });
        }
        Some(_) => return Err(()),
    }
    if content.is_empty() {
        return Ok(vec![projected_codex_source_event(
            record,
            Some(payload),
            source_type,
            ImportedSourceAttestation::NotAttested,
        )?]);
    }
    let source = projected_codex_source_metadata(
        record,
        Some(payload),
        ImportedSourceAttestation::NotAttested,
    )?;
    Ok(content
        .into_iter()
        .map(|content| ProjectedEntry {
            source_speaker: ImportedSourceAttestation::NotAttested,
            content,
            source: source.clone(),
        })
        .collect())
}

fn projected_codex_reasoning_blocks(
    payload: &[ImportedStructuredObjectMember],
    field: &str,
    content: &mut Vec<ImportedTranscriptContent>,
) -> Result<(), ()> {
    let Some(value) = unique_structured_field(payload, field)? else {
        return Ok(());
    };
    let blocks = match value {
        ImportedStructuredValue::Null => return Ok(()),
        ImportedStructuredValue::Array(blocks) => blocks,
        _ => return Err(()),
    };
    for block in blocks {
        let ImportedStructuredValue::Object(members) = block else {
            return Err(());
        };
        let source_type = projected_text_attestation(members, "type")?;
        content.push(match &source_type {
            ImportedSourceAttestation::Attested(value)
                if matches!(value.as_str(), "summary_text" | "reasoning_text" | "text") =>
            {
                ImportedTranscriptContent::Thinking {
                    thinking: projected_text_attestation(members, "text")?,
                    signature: ImportedSourceAttestation::NotAttested,
                }
            }
            ImportedSourceAttestation::Attested(_)
            | ImportedSourceAttestation::AttestedAbsent
            | ImportedSourceAttestation::NotAttested => {
                ImportedTranscriptContent::SourceMessageBlock { source_type }
            }
        });
    }
    Ok(())
}

fn project_codex_named_tool_call(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
) -> Result<Vec<ProjectedEntry>, ()> {
    let payload_type = projected_text_attestation(payload, "type")?;
    let input_field = match &payload_type {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "function_call" => {
            "arguments"
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "custom_tool_call" => {
            "input"
        }
        _ => return Err(()),
    };
    projected_codex_typed_entry(
        record,
        payload,
        ImportedTranscriptContent::ToolCall {
            source_call_id: projected_text_attestation(payload, "call_id")?,
            name: projected_text_attestation(payload, "name")?,
            input: projected_string_structured_attestation(payload, input_field)?,
            caller: ImportedSourceAttestation::NotAttested,
        },
    )
    .map(|entry| vec![entry])
}

fn project_codex_structured_tool_call(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    input_field: &str,
) -> Result<Vec<ProjectedEntry>, ()> {
    projected_codex_typed_entry(
        record,
        payload,
        ImportedTranscriptContent::ToolCall {
            source_call_id: projected_text_attestation(payload, "call_id")?,
            name: ImportedSourceAttestation::NotAttested,
            input: projected_structured_attestation(payload, input_field)?,
            caller: ImportedSourceAttestation::NotAttested,
        },
    )
    .map(|entry| vec![entry])
}

fn project_codex_web_search_call(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
) -> Result<Vec<ProjectedEntry>, ()> {
    projected_codex_typed_entry(
        record,
        payload,
        ImportedTranscriptContent::ToolCall {
            source_call_id: projected_text_attestation(payload, "id")?,
            name: ImportedSourceAttestation::NotAttested,
            input: projected_structured_attestation(payload, "action")?,
            caller: ImportedSourceAttestation::NotAttested,
        },
    )
    .map(|entry| vec![entry])
}

fn project_codex_tool_result(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
) -> Result<Vec<ProjectedEntry>, ()> {
    let content = match unique_structured_field(payload, "output")? {
        None => ImportedSourceAttestation::NotAttested,
        Some(ImportedStructuredValue::Null) => ImportedSourceAttestation::AttestedAbsent,
        Some(ImportedStructuredValue::String(value)) => {
            ImportedSourceAttestation::Attested(ImportedToolResultValue::Text(value.clone()))
        }
        Some(ImportedStructuredValue::Array(blocks)) => {
            let blocks = blocks
                .iter()
                .map(projected_codex_tool_result_block)
                .collect::<Result<Vec<_>, _>>()?;
            ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                blocks.into_boxed_slice(),
            ))
        }
        Some(_) => return Err(()),
    };
    projected_codex_typed_entry(
        record,
        payload,
        ImportedTranscriptContent::ToolResult {
            source_call_id: projected_text_attestation(payload, "call_id")?,
            content,
            is_error: ImportedSourceAttestation::NotAttested,
        },
    )
    .map(|entry| vec![entry])
}

fn projected_codex_tool_result_block(
    value: &ImportedStructuredValue,
) -> Result<ImportedToolResultBlock, ()> {
    let ImportedStructuredValue::Object(members) = value else {
        return Err(());
    };
    let source_type = projected_text_attestation(members, "type")?;
    match &source_type {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "input_text" => Ok(
            ImportedToolResultBlock::Text(projected_text_attestation(members, "text")?),
        ),
        ImportedSourceAttestation::Attested(value) if value.as_str() == "input_image" => {
            Ok(ImportedToolResultBlock::Image(
                projected_codex_image_attestation(members, source_type.clone())?,
            ))
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => {
            Ok(ImportedToolResultBlock::SourceResultBlock { source_type })
        }
    }
}

fn project_codex_tool_search_result(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
) -> Result<Vec<ProjectedEntry>, ()> {
    let content = match unique_structured_field(payload, "tools")? {
        None => ImportedSourceAttestation::NotAttested,
        Some(ImportedStructuredValue::Null) => ImportedSourceAttestation::AttestedAbsent,
        Some(ImportedStructuredValue::Array(tools)) => {
            let blocks = tools
                .iter()
                .map(|tool| {
                    let source_type = match tool {
                        ImportedStructuredValue::Object(members) => {
                            projected_text_attestation(members, "type")?
                        }
                        _ => ImportedSourceAttestation::NotAttested,
                    };
                    Ok(ImportedToolResultBlock::SourceResultBlock { source_type })
                })
                .collect::<Result<Vec<_>, ()>>()?;
            ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                blocks.into_boxed_slice(),
            ))
        }
        Some(_) => return Err(()),
    };
    projected_codex_typed_entry(
        record,
        payload,
        ImportedTranscriptContent::ToolResult {
            source_call_id: projected_text_attestation(payload, "call_id")?,
            content,
            is_error: ImportedSourceAttestation::NotAttested,
        },
    )
    .map(|entry| vec![entry])
}

fn projected_codex_typed_entry(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    content: ImportedTranscriptContent,
) -> Result<ProjectedEntry, ()> {
    Ok(ProjectedEntry {
        source_speaker: ImportedSourceAttestation::NotAttested,
        content,
        source: projected_codex_source_metadata(
            record,
            Some(payload),
            ImportedSourceAttestation::NotAttested,
        )?,
    })
}

fn projected_codex_source_event(
    record: &[ImportedStructuredObjectMember],
    payload: Option<&[ImportedStructuredObjectMember]>,
    source_type: ImportedSourceAttestation<ImportedText>,
    message_role: ImportedSourceAttestation<ImportedSpeaker>,
) -> Result<ProjectedEntry, ()> {
    Ok(ProjectedEntry {
        source_speaker: ImportedSourceAttestation::NotAttested,
        content: ImportedTranscriptContent::SourceEvent { source_type },
        source: projected_codex_source_metadata(record, payload, message_role)?,
    })
}

fn projected_codex_source_metadata(
    record: &[ImportedStructuredObjectMember],
    payload: Option<&[ImportedStructuredObjectMember]>,
    message_role: ImportedSourceAttestation<ImportedSpeaker>,
) -> Result<ImportedSourceMetadata, ()> {
    let record_id = payload
        .map(|payload| projected_text_attestation(payload, "id"))
        .transpose()?
        .unwrap_or(ImportedSourceAttestation::NotAttested);
    let source_session_id = payload
        .map(|payload| projected_text_attestation(payload, "session_id"))
        .transpose()?
        .unwrap_or(ImportedSourceAttestation::NotAttested);
    Ok(ImportedSourceMetadata::new(
        record_id,
        ImportedSourceAttestation::NotAttested,
        source_session_id,
        projected_text_attestation(record, "timestamp")?,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        message_role,
    ))
}

fn projected_codex_optional_object<'members>(
    members: &'members [ImportedStructuredObjectMember],
    name: &str,
) -> Result<Option<&'members [ImportedStructuredObjectMember]>, ()> {
    match unique_structured_field(members, name)? {
        Some(ImportedStructuredValue::Object(value)) => Ok(Some(value)),
        None | Some(_) => Ok(None),
    }
}

fn projected_codex_image_attestation(
    members: &[ImportedStructuredObjectMember],
    source_type: ImportedSourceAttestation<ImportedText>,
) -> Result<ImportedSourceAttestation<ImportedMediaSource>, ()> {
    Ok(ImportedSourceAttestation::Attested(
        ImportedMediaSource::new(
            source_type,
            ImportedSourceAttestation::NotAttested,
            projected_text_attestation(members, "image_url")?,
        ),
    ))
}
