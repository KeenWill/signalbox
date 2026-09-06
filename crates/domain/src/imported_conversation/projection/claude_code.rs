//! Claude Code session record projection for `docs/spec/conversation-import.md`.

use super::super::content::ImportedMessageContentAbsence;
use super::super::content::ImportedSourceMetadata;
use super::super::content::ImportedSpeaker;
use super::super::content::ImportedToolResultBlock;
use super::super::content::ImportedToolResultValue;
use super::super::content::ImportedTranscriptContent;
use super::super::structured_field::projected_bool_attestation;
use super::super::structured_field::projected_media_source_attestation;
use super::super::structured_field::projected_structured_attestation;
use super::super::structured_field::projected_text_attestation;
use super::super::structured_field::unique_structured_field;
use super::super::structured_value::ImportedSourceAttestation;
use super::super::structured_value::ImportedStructuredObjectMember;
use super::super::structured_value::ImportedStructuredValue;
use super::ClaudeCodeProjectionVersion;
use super::ProjectedEntry;

pub(super) fn project_claude_code_record(
    normalized: &ImportedStructuredValue,
    version: ClaudeCodeProjectionVersion,
) -> Result<Vec<ProjectedEntry>, ()> {
    let ImportedStructuredValue::Object(record) = normalized else {
        return Err(());
    };
    let source_type = projected_text_attestation(record, "type")?;
    let speaker = match &source_type {
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
        return Ok(vec![ProjectedEntry {
            source_speaker: ImportedSourceAttestation::NotAttested,
            content: ImportedTranscriptContent::SourceEvent { source_type },
            source: projected_source_metadata(record, ImportedSourceAttestation::NotAttested)?,
        }]);
    };

    let message = unique_structured_field(record, "message")?;
    let (content, message_role) = match message {
        None => (
            vec![ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::MessageNotAttested,
            )],
            ImportedSourceAttestation::NotAttested,
        ),
        Some(ImportedStructuredValue::Null) => (
            vec![ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::MessageAttestedAbsent,
            )],
            ImportedSourceAttestation::NotAttested,
        ),
        Some(ImportedStructuredValue::Object(message)) => {
            let role = projected_message_role(message)?;
            if let ImportedSourceAttestation::Attested(role) = role
                && role != speaker
            {
                return Err(());
            }
            (projected_message_content(message, version)?, role)
        }
        Some(_) => return Err(()),
    };
    let source = projected_source_metadata(record, message_role)?;
    Ok(content
        .into_iter()
        .map(|content| ProjectedEntry {
            source_speaker: ImportedSourceAttestation::Attested(speaker),
            content,
            source: source.clone(),
        })
        .collect())
}

fn projected_message_role(
    message: &[ImportedStructuredObjectMember],
) -> Result<ImportedSourceAttestation<ImportedSpeaker>, ()> {
    match unique_structured_field(message, "role")? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(ImportedStructuredValue::String(value)) if value.as_str() == "user" => {
            Ok(ImportedSourceAttestation::Attested(ImportedSpeaker::User))
        }
        Some(ImportedStructuredValue::String(value)) if value.as_str() == "assistant" => Ok(
            ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
        ),
        Some(_) => Err(()),
    }
}

fn projected_message_content(
    message: &[ImportedStructuredObjectMember],
    version: ClaudeCodeProjectionVersion,
) -> Result<Vec<ImportedTranscriptContent>, ()> {
    match unique_structured_field(message, "content")? {
        None => Ok(vec![ImportedTranscriptContent::MessageContentAbsent(
            ImportedMessageContentAbsence::ContentNotAttested,
        )]),
        Some(ImportedStructuredValue::Null) => {
            Ok(vec![ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::ContentAttestedAbsent,
            )])
        }
        Some(ImportedStructuredValue::String(value)) => Ok(vec![ImportedTranscriptContent::Text(
            ImportedSourceAttestation::Attested(value.clone()),
        )]),
        Some(ImportedStructuredValue::Array(blocks)) if blocks.is_empty() => {
            Ok(vec![ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::EmptyBlockArray,
            )])
        }
        Some(ImportedStructuredValue::Array(blocks)) => blocks
            .iter()
            .map(|block| match version {
                ClaudeCodeProjectionVersion::One => projected_content_block_v1(block),
                ClaudeCodeProjectionVersion::Two => projected_content_block_v2(block),
            })
            .collect::<Result<Vec<_>, _>>(),
        Some(_) => Err(()),
    }
}

fn projected_content_block_v1(
    value: &ImportedStructuredValue,
) -> Result<ImportedTranscriptContent, ()> {
    let ImportedStructuredValue::Object(members) = value else {
        return Err(());
    };
    match projected_required_type(members)? {
        "text" => Ok(ImportedTranscriptContent::Text(projected_text_attestation(
            members, "text",
        )?)),
        "tool_use" => Ok(ImportedTranscriptContent::ToolCall {
            source_call_id: projected_text_attestation(members, "id")?,
            name: projected_text_attestation(members, "name")?,
            input: projected_structured_attestation(members, "input")?,
            caller: projected_structured_attestation(members, "caller")?,
        }),
        "tool_result" => projected_tool_result(members, ClaudeCodeProjectionVersion::One),
        "thinking" => Ok(ImportedTranscriptContent::Thinking {
            thinking: projected_text_attestation(members, "thinking")?,
            signature: projected_text_attestation(members, "signature")?,
        }),
        "redacted_thinking" => Ok(ImportedTranscriptContent::RedactedThinking {
            data: projected_text_attestation(members, "data")?,
        }),
        "document" => Ok(ImportedTranscriptContent::Document {
            source: projected_media_source_attestation(members, "source")?,
        }),
        "fallback" => Ok(ImportedTranscriptContent::SourceMessageBlock {
            source_type: projected_text_attestation(members, "type")?,
        }),
        _ => Err(()),
    }
}

fn projected_content_block_v2(
    value: &ImportedStructuredValue,
) -> Result<ImportedTranscriptContent, ()> {
    let ImportedStructuredValue::Object(members) = value else {
        return Err(());
    };
    let source_type = projected_text_attestation(members, "type")?;
    match &source_type {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "text" => Ok(
            ImportedTranscriptContent::Text(projected_text_attestation(members, "text")?),
        ),
        ImportedSourceAttestation::Attested(value) if value.as_str() == "tool_use" => {
            Ok(ImportedTranscriptContent::ToolCall {
                source_call_id: projected_text_attestation(members, "id")?,
                name: projected_text_attestation(members, "name")?,
                input: projected_structured_attestation(members, "input")?,
                caller: projected_structured_attestation(members, "caller")?,
            })
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "tool_result" => {
            projected_tool_result(members, ClaudeCodeProjectionVersion::Two)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "thinking" => {
            Ok(ImportedTranscriptContent::Thinking {
                thinking: projected_text_attestation(members, "thinking")?,
                signature: projected_text_attestation(members, "signature")?,
            })
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "redacted_thinking" => {
            Ok(ImportedTranscriptContent::RedactedThinking {
                data: projected_text_attestation(members, "data")?,
            })
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "document" => {
            Ok(ImportedTranscriptContent::Document {
                source: projected_media_source_attestation(members, "source")?,
            })
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => {
            Ok(ImportedTranscriptContent::SourceMessageBlock { source_type })
        }
    }
}

fn projected_tool_result(
    members: &[ImportedStructuredObjectMember],
    version: ClaudeCodeProjectionVersion,
) -> Result<ImportedTranscriptContent, ()> {
    let content = match unique_structured_field(members, "content")? {
        None => ImportedSourceAttestation::NotAttested,
        Some(ImportedStructuredValue::Null) => ImportedSourceAttestation::AttestedAbsent,
        Some(ImportedStructuredValue::String(value)) => {
            ImportedSourceAttestation::Attested(ImportedToolResultValue::Text(value.clone()))
        }
        Some(ImportedStructuredValue::Array(blocks)) => {
            let blocks = blocks
                .iter()
                .map(|block| match version {
                    ClaudeCodeProjectionVersion::One => projected_tool_result_block_v1(block),
                    ClaudeCodeProjectionVersion::Two => projected_tool_result_block_v2(block),
                })
                .collect::<Result<Vec<_>, _>>()?;
            ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                blocks.into_boxed_slice(),
            ))
        }
        Some(_) => return Err(()),
    };
    Ok(ImportedTranscriptContent::ToolResult {
        source_call_id: projected_text_attestation(members, "tool_use_id")?,
        content,
        is_error: projected_bool_attestation(members, "is_error")?,
    })
}

fn projected_tool_result_block_v1(
    value: &ImportedStructuredValue,
) -> Result<ImportedToolResultBlock, ()> {
    let ImportedStructuredValue::Object(members) = value else {
        return Err(());
    };
    match projected_required_type(members)? {
        "text" => Ok(ImportedToolResultBlock::Text(projected_text_attestation(
            members, "text",
        )?)),
        "image" => Ok(ImportedToolResultBlock::Image(
            projected_media_source_attestation(members, "source")?,
        )),
        "tool_reference" => Ok(ImportedToolResultBlock::ToolReference {
            tool_name: projected_text_attestation(members, "tool_name")?,
        }),
        _ => Err(()),
    }
}

fn projected_tool_result_block_v2(
    value: &ImportedStructuredValue,
) -> Result<ImportedToolResultBlock, ()> {
    let ImportedStructuredValue::Object(members) = value else {
        return Err(());
    };
    let source_type = projected_text_attestation(members, "type")?;
    match &source_type {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "text" => Ok(
            ImportedToolResultBlock::Text(projected_text_attestation(members, "text")?),
        ),
        ImportedSourceAttestation::Attested(value) if value.as_str() == "image" => Ok(
            ImportedToolResultBlock::Image(projected_media_source_attestation(members, "source")?),
        ),
        ImportedSourceAttestation::Attested(value) if value.as_str() == "tool_reference" => {
            Ok(ImportedToolResultBlock::ToolReference {
                tool_name: projected_text_attestation(members, "tool_name")?,
            })
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => {
            Ok(ImportedToolResultBlock::SourceResultBlock { source_type })
        }
    }
}

fn projected_required_type(members: &[ImportedStructuredObjectMember]) -> Result<&str, ()> {
    match unique_structured_field(members, "type")? {
        Some(ImportedStructuredValue::String(value)) => Ok(value.as_str()),
        None | Some(_) => Err(()),
    }
}

fn projected_source_metadata(
    record: &[ImportedStructuredObjectMember],
    message_role: ImportedSourceAttestation<ImportedSpeaker>,
) -> Result<ImportedSourceMetadata, ()> {
    Ok(ImportedSourceMetadata::new(
        projected_text_attestation(record, "uuid")?,
        projected_text_attestation(record, "parentUuid")?,
        projected_text_attestation(record, "sessionId")?,
        projected_text_attestation(record, "timestamp")?,
        projected_bool_attestation(record, "isSidechain")?,
        projected_bool_attestation(record, "isMeta")?,
        message_role,
    ))
}
