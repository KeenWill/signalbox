//! Maximum-fidelity Claude Code session JSONL ingestion.
//!
//! The converter accepts caller-supplied bytes and identities, preserves every
//! raw JSONL record, and emits source-neutral imported entries. It performs no
//! filesystem access and creates no native Signalbox session.

mod json;

use std::{error::Error, fmt};

use json::{JsonFailure, parse_record};
use signalbox_application::ImportedConversationConverter;
use signalbox_domain::{
    ImportedConversation, ImportedConversationFormat, ImportedConversationId,
    ImportedConversationReconstitutionFailure, ImportedMediaSource, ImportedMessageContentAbsence,
    ImportedRawRecordPosition, ImportedRawSourceRecord, ImportedRecordEntryPosition,
    ImportedSourceAttestation, ImportedSourceMetadata, ImportedSpeaker,
    ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText, ImportedToolResultBlock,
    ImportedToolResultValue, ImportedTranscriptContent, ImportedTranscriptEntryId,
    ImportedTranscriptEntryInput, ImportedTranscriptPosition,
};

const FORMAT: ImportedConversationFormat = ImportedConversationFormat::ClaudeCodeSessionJsonlV1;

/// Claude Code session JSONL version 1 converter.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeCodeJsonlConverter;

/// Content-silent reason a complete Claude Code JSONL conversion failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaudeCodeJsonlConversionFailure {
    /// The supplied byte sequence contained no JSONL record.
    EmptySource,
    /// A physical JSONL record was empty.
    BlankLine {
        /// One-based physical line number.
        line: u64,
    },
    /// One record was not valid UTF-8.
    InvalidUtf8 {
        /// One-based physical line number.
        line: u64,
    },
    /// One record was not valid JSON.
    InvalidJson {
        /// One-based physical line number.
        line: u64,
    },
    /// One record exceeded 128 nested array or object containers.
    JsonDepthExceeded {
        /// One-based physical line number.
        line: u64,
    },
    /// One JSONL record was not an object.
    TopLevelNotObject {
        /// One-based physical line number.
        line: u64,
    },
    /// The top-level source type was present with an unsupported value shape.
    InvalidRecordType {
        /// One-based physical line number.
        line: u64,
    },
    /// Modeled source-envelope metadata had an unsupported value shape.
    InvalidSourceMetadata {
        /// One-based physical line number.
        line: u64,
    },
    /// A user or assistant message envelope had an unsupported value shape.
    InvalidMessageEnvelope {
        /// One-based physical line number.
        line: u64,
    },
    /// A nested message role had an unsupported value or shape.
    InvalidMessageRole {
        /// One-based physical line number.
        line: u64,
    },
    /// A nested message role contradicted its top-level source speaker.
    MessageRoleMismatch {
        /// One-based physical line number.
        line: u64,
    },
    /// Message content was neither a string, an array, null, nor omitted.
    InvalidMessageContent {
        /// One-based physical line number.
        line: u64,
    },
    /// One message content block had an unsupported shape.
    InvalidContentBlock {
        /// One-based physical line number.
        line: u64,
        /// One-based block position inside message content.
        block: u64,
    },
    /// One message content block named an unsupported type.
    UnknownContentBlockType {
        /// One-based physical line number.
        line: u64,
        /// One-based block position inside message content.
        block: u64,
    },
    /// One tool-result block had an unsupported shape.
    InvalidToolResultBlock {
        /// One-based physical line number.
        line: u64,
        /// One-based message block position.
        block: u64,
        /// One-based tool-result block position.
        result_block: u64,
    },
    /// One tool-result block named an unsupported type.
    UnknownToolResultBlockType {
        /// One-based physical line number.
        line: u64,
        /// One-based message block position.
        block: u64,
        /// One-based tool-result block position.
        result_block: u64,
    },
    /// A required source or entry position could not be represented.
    PositionExhausted,
    /// The converted candidate violated an imported-conversation invariant.
    InvalidAggregate(ImportedConversationReconstitutionFailure),
}

/// A complete Claude Code JSONL conversion failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaudeCodeJsonlConversionError {
    failure: ClaudeCodeJsonlConversionFailure,
}

impl ClaudeCodeJsonlConversionError {
    /// Returns the content-silent failure classification.
    pub const fn failure(self) -> ClaudeCodeJsonlConversionFailure {
        self.failure
    }
}

impl fmt::Display for ClaudeCodeJsonlConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Claude Code JSONL conversion failed")
    }
}

impl Error for ClaudeCodeJsonlConversionError {}

impl ImportedConversationConverter for ClaudeCodeJsonlConverter {
    type Error = ClaudeCodeJsonlConversionError;

    fn format(&self) -> ImportedConversationFormat {
        FORMAT
    }

    fn convert<NextEntryId>(
        &mut self,
        conversation: ImportedConversationId,
        source: &[u8],
        mut next_entry_id: NextEntryId,
    ) -> Result<ImportedConversation, Self::Error>
    where
        NextEntryId: FnMut() -> ImportedTranscriptEntryId,
    {
        let records = split_records(source)?;
        let mut raws = Vec::with_capacity(records.len());
        let mut pending_records = Vec::with_capacity(records.len());
        for (index, bytes) in records.into_iter().enumerate() {
            let line = ordinal(index)?;
            let normalized = parse_record(bytes).map_err(|failure| json_error(line, failure))?;
            let pending = normalize_record(&normalized, line)?;
            raws.push(ImportedRawSourceRecord::from_converted(
                bytes.to_vec(),
                normalized,
            ));
            pending_records.push(pending);
        }

        let entry_capacity = pending_records
            .iter()
            .try_fold(0_usize, |total, record| total.checked_add(record.len()));
        let entry_count = entry_capacity.ok_or_else(position_error)?;
        let mut entries = Vec::with_capacity(entry_count);
        let mut global_position = ImportedTranscriptPosition::first();
        let mut emitted_entries = 0_usize;
        for (raw_index, pending) in pending_records.into_iter().enumerate() {
            let raw_position = ImportedRawRecordPosition::try_from_u64(ordinal(raw_index)?)
                .ok_or_else(position_error)?;
            let pending_count = pending.len();
            let mut within_position = ImportedRecordEntryPosition::first();
            for (entry_index, pending_entry) in pending.into_iter().enumerate() {
                entries.push(ImportedTranscriptEntryInput::new(
                    next_entry_id(),
                    conversation,
                    global_position,
                    raw_position,
                    within_position,
                    pending_entry.source_speaker,
                    pending_entry.content,
                    pending_entry.source,
                ));
                emitted_entries = emitted_entries.checked_add(1).ok_or_else(position_error)?;
                if entry_index + 1 < pending_count {
                    within_position = within_position.checked_next().ok_or_else(position_error)?;
                }
                if emitted_entries < entry_count {
                    global_position = global_position.checked_next().ok_or_else(position_error)?;
                }
            }
        }

        ImportedConversation::from_converted_records(conversation, FORMAT, raws, entries).map_err(
            |error| ClaudeCodeJsonlConversionError {
                failure: ClaudeCodeJsonlConversionFailure::InvalidAggregate(error.failure()),
            },
        )
    }
}

fn position_error() -> ClaudeCodeJsonlConversionError {
    ClaudeCodeJsonlConversionError {
        failure: ClaudeCodeJsonlConversionFailure::PositionExhausted,
    }
}

fn ordinal(index: usize) -> Result<u64, ClaudeCodeJsonlConversionError> {
    u64::try_from(index)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(position_error)
}

fn json_error(line: u64, failure: JsonFailure) -> ClaudeCodeJsonlConversionError {
    let failure = match failure {
        JsonFailure::InvalidUtf8 => ClaudeCodeJsonlConversionFailure::InvalidUtf8 { line },
        JsonFailure::Syntax => ClaudeCodeJsonlConversionFailure::InvalidJson { line },
        JsonFailure::DepthExceeded => ClaudeCodeJsonlConversionFailure::JsonDepthExceeded { line },
    };
    ClaudeCodeJsonlConversionError { failure }
}

fn conversion_error(failure: ClaudeCodeJsonlConversionFailure) -> ClaudeCodeJsonlConversionError {
    ClaudeCodeJsonlConversionError { failure }
}

fn split_records(source: &[u8]) -> Result<Vec<&[u8]>, ClaudeCodeJsonlConversionError> {
    if source.is_empty() {
        return Err(conversion_error(
            ClaudeCodeJsonlConversionFailure::EmptySource,
        ));
    }
    let mut records = Vec::new();
    let mut start = 0_usize;
    let mut line_index = 0_usize;
    loop {
        let remaining = source.get(start..).ok_or_else(position_error)?;
        let newline = remaining.iter().position(|byte| *byte == b'\n');
        let (end, terminal) = match newline {
            Some(offset) => (start.checked_add(offset).ok_or_else(position_error)?, false),
            None => (source.len(), true),
        };
        let record_end = if end > start && source.get(end - 1) == Some(&b'\r') {
            end - 1
        } else {
            end
        };
        if record_end == start {
            return Err(conversion_error(
                ClaudeCodeJsonlConversionFailure::BlankLine {
                    line: ordinal(line_index)?,
                },
            ));
        }
        records.push(source.get(start..record_end).ok_or_else(position_error)?);
        line_index = line_index.checked_add(1).ok_or_else(position_error)?;
        if terminal {
            break;
        }
        start = end.checked_add(1).ok_or_else(position_error)?;
        if start == source.len() {
            break;
        }
    }
    Ok(records)
}

struct PendingEntry {
    source_speaker: ImportedSourceAttestation<ImportedSpeaker>,
    content: ImportedTranscriptContent,
    source: ImportedSourceMetadata,
}

fn normalize_record(
    normalized: &ImportedStructuredValue,
    line: u64,
) -> Result<Vec<PendingEntry>, ClaudeCodeJsonlConversionError> {
    let ImportedStructuredValue::Object(record) = normalized else {
        return Err(conversion_error(
            ClaudeCodeJsonlConversionFailure::TopLevelNotObject { line },
        ));
    };
    let source_type = text_attestation(record, "type").map_err(|()| {
        conversion_error(ClaudeCodeJsonlConversionFailure::InvalidRecordType { line })
    })?;
    let speaker = match &source_type {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "user" => {
            Some(ImportedSpeaker::User)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "assistant" => {
            Some(ImportedSpeaker::Assistant)
        }
        _ => None,
    };
    match speaker {
        Some(speaker) => normalize_message_record(record, line, speaker),
        None => {
            let source = source_metadata(record, ImportedSourceAttestation::NotAttested, line)?;
            Ok(vec![PendingEntry {
                source_speaker: ImportedSourceAttestation::NotAttested,
                content: ImportedTranscriptContent::SourceEvent { source_type },
                source,
            }])
        }
    }
}

fn normalize_message_record(
    record: &[ImportedStructuredObjectMember],
    line: u64,
    speaker: ImportedSpeaker,
) -> Result<Vec<PendingEntry>, ClaudeCodeJsonlConversionError> {
    let message = unique_field(record, "message").map_err(|()| {
        conversion_error(ClaudeCodeJsonlConversionFailure::InvalidMessageEnvelope { line })
    })?;
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
            let role = message_role(message, line)?;
            if let ImportedSourceAttestation::Attested(role) = role
                && role != speaker
            {
                return Err(conversion_error(
                    ClaudeCodeJsonlConversionFailure::MessageRoleMismatch { line },
                ));
            }
            (message_content(message, line)?, role)
        }
        Some(_) => {
            return Err(conversion_error(
                ClaudeCodeJsonlConversionFailure::InvalidMessageEnvelope { line },
            ));
        }
    };
    let source = source_metadata(record, message_role, line)?;
    Ok(content
        .into_iter()
        .map(|content| PendingEntry {
            source_speaker: ImportedSourceAttestation::Attested(speaker),
            content,
            source: source.clone(),
        })
        .collect())
}

fn message_role(
    message: &[ImportedStructuredObjectMember],
    line: u64,
) -> Result<ImportedSourceAttestation<ImportedSpeaker>, ClaudeCodeJsonlConversionError> {
    match unique_field(message, "role").map_err(|()| {
        conversion_error(ClaudeCodeJsonlConversionFailure::InvalidMessageRole { line })
    })? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(ImportedStructuredValue::String(value)) if value.as_str() == "user" => {
            Ok(ImportedSourceAttestation::Attested(ImportedSpeaker::User))
        }
        Some(ImportedStructuredValue::String(value)) if value.as_str() == "assistant" => Ok(
            ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
        ),
        Some(_) => Err(conversion_error(
            ClaudeCodeJsonlConversionFailure::InvalidMessageRole { line },
        )),
    }
}

fn message_content(
    message: &[ImportedStructuredObjectMember],
    line: u64,
) -> Result<Vec<ImportedTranscriptContent>, ClaudeCodeJsonlConversionError> {
    match unique_field(message, "content").map_err(|()| {
        conversion_error(ClaudeCodeJsonlConversionFailure::InvalidMessageContent { line })
    })? {
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
            .enumerate()
            .map(|(index, block)| normalize_content_block(block, line, ordinal(index)?))
            .collect(),
        Some(_) => Err(conversion_error(
            ClaudeCodeJsonlConversionFailure::InvalidMessageContent { line },
        )),
    }
}

fn normalize_content_block(
    value: &ImportedStructuredValue,
    line: u64,
    block: u64,
) -> Result<ImportedTranscriptContent, ClaudeCodeJsonlConversionError> {
    let ImportedStructuredValue::Object(members) = value else {
        return Err(invalid_content_block(line, block));
    };
    let block_type = required_type(members).map_err(|failure| match failure {
        RequiredTypeFailure::Invalid => invalid_content_block(line, block),
        RequiredTypeFailure::Unknown => {
            conversion_error(ClaudeCodeJsonlConversionFailure::UnknownContentBlockType {
                line,
                block,
            })
        }
    })?;
    match block_type {
        "text" => Ok(ImportedTranscriptContent::Text(
            text_attestation(members, "text").map_err(|()| invalid_content_block(line, block))?,
        )),
        "tool_use" => Ok(ImportedTranscriptContent::ToolCall {
            source_call_id: text_attestation(members, "id")
                .map_err(|()| invalid_content_block(line, block))?,
            name: text_attestation(members, "name")
                .map_err(|()| invalid_content_block(line, block))?,
            input: structured_attestation(members, "input")
                .map_err(|()| invalid_content_block(line, block))?,
            caller: structured_attestation(members, "caller")
                .map_err(|()| invalid_content_block(line, block))?,
        }),
        "tool_result" => normalize_tool_result(members, line, block),
        "thinking" => Ok(ImportedTranscriptContent::Thinking {
            thinking: text_attestation(members, "thinking")
                .map_err(|()| invalid_content_block(line, block))?,
            signature: text_attestation(members, "signature")
                .map_err(|()| invalid_content_block(line, block))?,
        }),
        "redacted_thinking" => Ok(ImportedTranscriptContent::RedactedThinking {
            data: text_attestation(members, "data")
                .map_err(|()| invalid_content_block(line, block))?,
        }),
        "document" => Ok(ImportedTranscriptContent::Document {
            source: media_source_attestation(members, "source")
                .map_err(|()| invalid_content_block(line, block))?,
        }),
        _ => Err(conversion_error(
            ClaudeCodeJsonlConversionFailure::UnknownContentBlockType { line, block },
        )),
    }
}

fn normalize_tool_result(
    members: &[ImportedStructuredObjectMember],
    line: u64,
    block: u64,
) -> Result<ImportedTranscriptContent, ClaudeCodeJsonlConversionError> {
    let source_call_id = text_attestation(members, "tool_use_id")
        .map_err(|()| invalid_content_block(line, block))?;
    let is_error =
        bool_attestation(members, "is_error").map_err(|()| invalid_content_block(line, block))?;
    let content =
        match unique_field(members, "content").map_err(|()| invalid_content_block(line, block))? {
            None => ImportedSourceAttestation::NotAttested,
            Some(ImportedStructuredValue::Null) => ImportedSourceAttestation::AttestedAbsent,
            Some(ImportedStructuredValue::String(value)) => {
                ImportedSourceAttestation::Attested(ImportedToolResultValue::Text(value.clone()))
            }
            Some(ImportedStructuredValue::Array(blocks)) => {
                let blocks = blocks
                    .iter()
                    .enumerate()
                    .map(|(index, result)| {
                        normalize_tool_result_block(result, line, block, ordinal(index)?)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                    blocks.into_boxed_slice(),
                ))
            }
            Some(_) => return Err(invalid_content_block(line, block)),
        };
    Ok(ImportedTranscriptContent::ToolResult {
        source_call_id,
        content,
        is_error,
    })
}

fn normalize_tool_result_block(
    value: &ImportedStructuredValue,
    line: u64,
    block: u64,
    result_block: u64,
) -> Result<ImportedToolResultBlock, ClaudeCodeJsonlConversionError> {
    let ImportedStructuredValue::Object(members) = value else {
        return Err(invalid_tool_result_block(line, block, result_block));
    };
    let block_type = required_type(members).map_err(|failure| match failure {
        RequiredTypeFailure::Invalid => invalid_tool_result_block(line, block, result_block),
        RequiredTypeFailure::Unknown => conversion_error(
            ClaudeCodeJsonlConversionFailure::UnknownToolResultBlockType {
                line,
                block,
                result_block,
            },
        ),
    })?;
    match block_type {
        "text" => Ok(ImportedToolResultBlock::Text(
            text_attestation(members, "text")
                .map_err(|()| invalid_tool_result_block(line, block, result_block))?,
        )),
        "image" => Ok(ImportedToolResultBlock::Image(
            media_source_attestation(members, "source")
                .map_err(|()| invalid_tool_result_block(line, block, result_block))?,
        )),
        "tool_reference" => Ok(ImportedToolResultBlock::ToolReference {
            tool_name: text_attestation(members, "tool_name")
                .map_err(|()| invalid_tool_result_block(line, block, result_block))?,
        }),
        _ => Err(conversion_error(
            ClaudeCodeJsonlConversionFailure::UnknownToolResultBlockType {
                line,
                block,
                result_block,
            },
        )),
    }
}

fn invalid_content_block(line: u64, block: u64) -> ClaudeCodeJsonlConversionError {
    conversion_error(ClaudeCodeJsonlConversionFailure::InvalidContentBlock { line, block })
}

fn invalid_tool_result_block(
    line: u64,
    block: u64,
    result_block: u64,
) -> ClaudeCodeJsonlConversionError {
    conversion_error(ClaudeCodeJsonlConversionFailure::InvalidToolResultBlock {
        line,
        block,
        result_block,
    })
}

enum RequiredTypeFailure {
    Invalid,
    Unknown,
}

fn required_type(members: &[ImportedStructuredObjectMember]) -> Result<&str, RequiredTypeFailure> {
    match unique_field(members, "type").map_err(|()| RequiredTypeFailure::Invalid)? {
        Some(ImportedStructuredValue::String(value)) => Ok(value.as_str()),
        None | Some(ImportedStructuredValue::Null) => Err(RequiredTypeFailure::Unknown),
        Some(_) => Err(RequiredTypeFailure::Invalid),
    }
}

fn source_metadata(
    record: &[ImportedStructuredObjectMember],
    message_role: ImportedSourceAttestation<ImportedSpeaker>,
    line: u64,
) -> Result<ImportedSourceMetadata, ClaudeCodeJsonlConversionError> {
    let invalid =
        |()| conversion_error(ClaudeCodeJsonlConversionFailure::InvalidSourceMetadata { line });
    Ok(ImportedSourceMetadata::new(
        text_attestation(record, "uuid").map_err(invalid)?,
        text_attestation(record, "parentUuid").map_err(invalid)?,
        text_attestation(record, "sessionId").map_err(invalid)?,
        text_attestation(record, "timestamp").map_err(invalid)?,
        bool_attestation(record, "isSidechain").map_err(invalid)?,
        bool_attestation(record, "isMeta").map_err(invalid)?,
        message_role,
    ))
}

fn text_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedText>, ()> {
    match unique_field(members, name)? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(ImportedStructuredValue::String(value)) => {
            Ok(ImportedSourceAttestation::Attested(value.clone()))
        }
        Some(_) => Err(()),
    }
}

fn bool_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<bool>, ()> {
    match unique_field(members, name)? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(ImportedStructuredValue::Boolean(value)) => {
            Ok(ImportedSourceAttestation::Attested(*value))
        }
        Some(_) => Err(()),
    }
}

fn structured_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedStructuredValue>, ()> {
    match unique_field(members, name)? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(value) => Ok(ImportedSourceAttestation::Attested(value.clone())),
    }
}

fn media_source_attestation(
    members: &[ImportedStructuredObjectMember],
    name: &str,
) -> Result<ImportedSourceAttestation<ImportedMediaSource>, ()> {
    match unique_field(members, name)? {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(ImportedStructuredValue::Object(source)) => Ok(ImportedSourceAttestation::Attested(
            ImportedMediaSource::new(
                text_attestation(source, "type")?,
                text_attestation(source, "media_type")?,
                text_attestation(source, "data")?,
            ),
        )),
        Some(_) => Err(()),
    }
}

fn unique_field<'members>(
    members: &'members [ImportedStructuredObjectMember],
    name: &str,
) -> Result<Option<&'members ImportedStructuredValue>, ()> {
    let mut found = None;
    for member in members {
        if member.name().as_str() == name {
            if found.is_some() {
                return Err(());
            }
            found = Some(member.value());
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use signalbox_application::ImportedConversationConverter;
    use signalbox_domain::{
        ImportedConversationId, ImportedMessageContentAbsence, ImportedSourceAttestation,
        ImportedSpeaker, ImportedToolResultBlock, ImportedToolResultValue,
        ImportedTranscriptContent, ImportedTranscriptEntryId,
    };
    use uuid::Uuid;

    use super::{ClaudeCodeJsonlConversionFailure, ClaudeCodeJsonlConverter};

    fn conversation() -> ImportedConversationId {
        ImportedConversationId::from_uuid(Uuid::from_u128(1))
    }

    fn ids(count: usize) -> VecDeque<ImportedTranscriptEntryId> {
        (0..count)
            .map(|index| {
                ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(
                    u128::try_from(index)
                        .ok()
                        .and_then(|value| value.checked_add(100))
                        .unwrap_or(u128::MAX),
                ))
            })
            .collect()
    }

    #[test]
    fn inv038_converts_every_observed_content_kind_and_frontier() {
        let source = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"value\":1e+09}\n",
            "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,",
            "\"sessionId\":\"s\",\"timestamp\":\"t\",\"isSidechain\":true,",
            "\"isMeta\":false,\"message\":{\"role\":\"user\",\"content\":[",
            "{\"type\":\"text\",\"text\":\"ask\\u0000\"},",
            "{\"type\":\"tool_result\",\"tool_use_id\":\"call\",\"is_error\":false,",
            "\"content\":[{\"type\":\"text\",\"text\":null},",
            "{\"type\":\"image\",\"source\":{\"type\":\"base64\",",
            "\"media_type\":\"image/png\",\"data\":\"AA==\"}},",
            "{\"type\":\"tool_reference\",\"tool_name\":\"lookup\"}]}]}}\r\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[",
            "{\"type\":\"thinking\",\"thinking\":\"private\",\"signature\":\"sig\"},",
            "{\"type\":\"redacted_thinking\",\"data\":\"sealed\"},",
            "{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"lookup\",",
            "\"input\":{\"same\":1,\"same\":2},\"caller\":{\"kind\":\"direct\"}},",
            "{\"type\":\"document\",\"source\":{\"type\":\"base64\",",
            "\"media_type\":\"application/pdf\",\"data\":\"AA==\"}},",
            "{\"type\":\"text\",\"text\":\"done\"}]}}"
        );
        let mut ids = ids(11);
        let imported = ClaudeCodeJsonlConverter
            .convert(conversation(), source.as_bytes(), || {
                ids.pop_front().unwrap_or_else(|| {
                    ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(u128::MAX))
                })
            })
            .unwrap_or_else(|_| panic!("synthetic full-vocabulary transcript should convert"));

        assert_eq!(imported.raw_records().len(), 3);
        assert_eq!(
            imported.raw_records()[1].bytes().last(),
            Some(&b'}'),
            "CRLF is a delimiter, not raw record content"
        );
        assert_eq!(imported.entries().len(), 8);
        assert_eq!(imported.frontiers().count(), imported.entries().len());
        for frontier in imported.frontiers() {
            assert_eq!(
                imported
                    .prefix(frontier)
                    .map(<[signalbox_domain::ImportedTranscriptEntry]>::len),
                usize::try_from(frontier.through_position().as_u64()).ok()
            );
        }
        assert!(matches!(
            imported.entries()[0].content(),
            ImportedTranscriptContent::SourceEvent { .. }
        ));
        assert!(matches!(
            imported.entries()[1].source_speaker(),
            ImportedSourceAttestation::Attested(ImportedSpeaker::User)
        ));
        let ImportedTranscriptContent::ToolResult { content, .. } = imported.entries()[2].content()
        else {
            panic!("third synthetic entry should be a tool result");
        };
        let ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(blocks)) = content
        else {
            panic!("synthetic tool result should retain its rich blocks");
        };
        assert!(matches!(
            &blocks[0],
            ImportedToolResultBlock::Text(ImportedSourceAttestation::AttestedAbsent)
        ));
    }

    #[test]
    fn preserves_each_distinct_message_absence() {
        let source = concat!(
            "{\"type\":\"user\"}\n",
            "{\"type\":\"user\",\"message\":null}\n",
            "{\"type\":\"user\",\"message\":{}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":null}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[]}}"
        );
        let mut ids = ids(5);
        let imported = ClaudeCodeJsonlConverter
            .convert(conversation(), source.as_bytes(), || {
                ids.pop_front().unwrap_or_else(|| {
                    ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(u128::MAX))
                })
            })
            .unwrap_or_else(|_| panic!("synthetic absence transcript should convert"));
        let expected = [
            ImportedMessageContentAbsence::MessageNotAttested,
            ImportedMessageContentAbsence::MessageAttestedAbsent,
            ImportedMessageContentAbsence::ContentNotAttested,
            ImportedMessageContentAbsence::ContentAttestedAbsent,
            ImportedMessageContentAbsence::EmptyBlockArray,
        ];
        for (entry, expected) in imported.entries().iter().zip(expected) {
            assert_eq!(
                entry.content(),
                &ImportedTranscriptContent::MessageContentAbsent(expected)
            );
        }
    }

    #[test]
    fn preserves_empty_and_absent_text_fields_and_source_only_files() {
        let source = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":[",
            "{\"type\":\"text\"},{\"type\":\"text\",\"text\":null},",
            "{\"type\":\"text\",\"text\":\"\"}]}}"
        );
        let mut ids = ids(3);
        let imported = ClaudeCodeJsonlConverter
            .convert(conversation(), source.as_bytes(), || {
                ids.pop_front().unwrap_or_else(|| {
                    ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(u128::MAX))
                })
            })
            .unwrap_or_else(|_| panic!("synthetic text attestations should convert"));
        assert!(matches!(
            imported.entries()[0].content(),
            ImportedTranscriptContent::Text(ImportedSourceAttestation::NotAttested)
        ));
        assert!(matches!(
            imported.entries()[1].content(),
            ImportedTranscriptContent::Text(ImportedSourceAttestation::AttestedAbsent)
        ));
        assert!(matches!(
            imported.entries()[2].content(),
            ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(value))
                if value.as_str().is_empty()
        ));

        let source_only = ClaudeCodeJsonlConverter
            .convert(
                conversation(),
                br#"{"type":"summary","value":null}"#,
                || ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(200)),
            )
            .unwrap_or_else(|_| panic!("synthetic source-only transcript should convert"));
        assert_eq!(source_only.entries().len(), 1);
        assert!(matches!(
            source_only.entries()[0].content(),
            ImportedTranscriptContent::SourceEvent { .. }
        ));
    }

    #[test]
    fn errors_are_content_silent_and_conversion_is_complete() {
        let source = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"secret-before\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[",
            "{\"type\":\"future-secret-kind\",\"payload\":\"secret-after\"}]}}"
        );
        let error = ClaudeCodeJsonlConverter
            .convert(conversation(), source.as_bytes(), || {
                ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100))
            })
            .expect_err("unknown synthetic block must fail the complete conversion");
        assert_eq!(
            error.failure(),
            ClaudeCodeJsonlConversionFailure::UnknownContentBlockType { line: 2, block: 1 }
        );
        let debug = format!("{error:?}");
        assert!(!debug.contains("secret"));
        assert_eq!(error.to_string(), "Claude Code JSONL conversion failed");
    }

    #[test]
    fn rejects_blank_lines_role_conflicts_and_duplicate_entry_ids() {
        let blank = ClaudeCodeJsonlConverter
            .convert(conversation(), b"{\"type\":\"system\"}\n\n", || {
                ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100))
            })
            .expect_err("blank synthetic JSONL line must fail");
        assert_eq!(
            blank.failure(),
            ClaudeCodeJsonlConversionFailure::BlankLine { line: 2 }
        );

        let mismatch = ClaudeCodeJsonlConverter
            .convert(
                conversation(),
                br#"{"type":"user","message":{"role":"assistant","content":"x"}}"#,
                || ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100)),
            )
            .expect_err("contradictory synthetic role must fail");
        assert_eq!(
            mismatch.failure(),
            ClaudeCodeJsonlConversionFailure::MessageRoleMismatch { line: 1 }
        );

        let duplicate = ClaudeCodeJsonlConverter
            .convert(
                conversation(),
                br#"{"type":"user","message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#,
                || ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100)),
            )
            .expect_err("duplicate synthetic entry identities must fail");
        assert!(matches!(
            duplicate.failure(),
            ClaudeCodeJsonlConversionFailure::InvalidAggregate(
                signalbox_domain::ImportedConversationReconstitutionFailure::DuplicateEntry { .. }
            )
        ));
    }
}
