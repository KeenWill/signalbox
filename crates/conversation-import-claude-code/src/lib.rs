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
        let record_end = if !terminal && end > start && source.get(end - 1) == Some(&b'\r') {
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
        "fallback" => Ok(ImportedTranscriptContent::SourceMessageBlock {
            source_type: text_attestation(members, "type")
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
        ImportedConversation, ImportedConversationId, ImportedMessageContentAbsence,
        ImportedSourceAttestation, ImportedSpeaker, ImportedToolResultBlock,
        ImportedToolResultValue, ImportedTranscriptContent, ImportedTranscriptEntry,
        ImportedTranscriptEntryId,
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

    #[track_caller]
    fn convert_synthetic(source: &str) -> ImportedConversation {
        let mut next_identity = 100_u128;
        ClaudeCodeJsonlConverter
            .convert(conversation(), source.as_bytes(), || {
                let identity = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(next_identity));
                next_identity = next_identity
                    .checked_add(1)
                    .expect("synthetic identity range is bounded");
                identity
            })
            .unwrap_or_else(|_| panic!("synthetic transcript should convert"))
    }

    #[track_caller]
    fn assert_message_absence(
        entry: &ImportedTranscriptEntry,
        expected: ImportedMessageContentAbsence,
    ) {
        assert_eq!(
            entry.content(),
            &ImportedTranscriptContent::MessageContentAbsent(expected)
        );
    }

    #[test]
    fn s28_inv038_crlf_delimiter_is_not_raw_record_content() {
        let imported =
            convert_synthetic("{\"type\":\"system\"}\r\n{\"type\":\"system\",\"value\":1}");

        assert_eq!(imported.raw_records()[0].bytes(), br#"{"type":"system"}"#);
    }

    #[test]
    fn s28_inv038_each_entry_boundary_resolves_its_exact_prefix() {
        let imported = convert_synthetic(
            "{\"type\":\"user\",\"message\":{\"content\":[\
             {\"type\":\"text\",\"text\":\"one\"},\
             {\"type\":\"text\",\"text\":\"two\"}]}}",
        );
        let mut frontiers = imported.frontiers();
        let first = frontiers.next().expect("first entry has a frontier");
        let second = frontiers.next().expect("second entry has a frontier");

        assert_eq!(imported.prefix(first), Some(&imported.entries()[..1]));
        assert_eq!(imported.prefix(second), Some(imported.entries()));
        assert_eq!(frontiers.next(), None);
    }

    #[test]
    fn s28_inv038_within_record_positions_follow_exact_sequence() {
        let imported = convert_synthetic(
            "{\"type\":\"user\",\"message\":{\"content\":[\
             {\"type\":\"text\",\"text\":\"one\"},\
             {\"type\":\"text\",\"text\":\"two\"}]}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":\"three\"}}",
        );

        assert_eq!(imported.entries()[0].record_entry_position().as_u64(), 1);
        assert_eq!(imported.entries()[1].record_entry_position().as_u64(), 2);
        assert_eq!(imported.entries()[2].record_entry_position().as_u64(), 1);
    }

    #[test]
    fn s28_inv038_maps_non_message_records_to_source_events() {
        let imported = convert_synthetic("{\"type\":\"system\",\"subtype\":\"init\"}");

        assert!(matches!(
            imported.entries()[0].content(),
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(value)
            } if value.as_str() == "system"
        ));
    }

    #[test]
    fn s28_inv038_attests_message_speaker_from_top_level_type() {
        let imported = convert_synthetic("{\"type\":\"user\",\"message\":{\"content\":\"ask\"}}");

        assert!(matches!(
            imported.entries()[0].source_speaker(),
            ImportedSourceAttestation::Attested(ImportedSpeaker::User)
        ));
    }

    #[test]
    fn s28_inv038_preserves_absent_text_inside_tool_result_blocks() {
        let imported = convert_synthetic(
            "{\"type\":\"user\",\"message\":{\"content\":[\
             {\"type\":\"tool_result\",\"content\":[\
             {\"type\":\"text\",\"text\":null}]}]}}",
        );
        let ImportedTranscriptContent::ToolResult { content, .. } = imported.entries()[0].content()
        else {
            panic!("synthetic entry should be a tool result");
        };
        let ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(blocks)) = content
        else {
            panic!("synthetic tool result should retain result blocks");
        };

        assert_eq!(
            blocks.as_ref(),
            [ImportedToolResultBlock::Text(
                ImportedSourceAttestation::AttestedAbsent
            )]
        );
    }

    #[test]
    fn s28_inv038_maps_fallback_to_source_message_block() {
        let imported = convert_synthetic(
            "{\"type\":\"assistant\",\"message\":{\"content\":[\
             {\"type\":\"fallback\",\"from\":{\"model\":\"before\"},\
             \"to\":{\"model\":\"after\"}}]}}",
        );

        assert!(matches!(
            imported.entries()[0].content(),
            ImportedTranscriptContent::SourceMessageBlock {
                source_type: ImportedSourceAttestation::Attested(value)
            } if value.as_str() == "fallback"
        ));
    }

    #[test]
    fn s28_inv038_maps_tool_call_fields_together() {
        let imported = convert_synthetic(
            "{\"type\":\"assistant\",\"message\":{\"content\":[\
             {\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"lookup\",\
             \"input\":{\"query\":\"x\"},\"caller\":{\"kind\":\"direct\"}}]}}",
        );
        let ImportedTranscriptContent::ToolCall {
            source_call_id,
            name,
            input,
            caller,
        } = imported.entries()[0].content()
        else {
            panic!("synthetic entry should be a tool call");
        };

        assert!(
            matches!(source_call_id, ImportedSourceAttestation::Attested(value) if value.as_str() == "call")
        );
        assert!(
            matches!(name, ImportedSourceAttestation::Attested(value) if value.as_str() == "lookup")
        );
        assert!(matches!(input, ImportedSourceAttestation::Attested(_)));
        assert!(matches!(caller, ImportedSourceAttestation::Attested(_)));
    }

    #[test]
    fn s28_inv038_maps_thinking_and_signature_together() {
        let imported = convert_synthetic(
            "{\"type\":\"assistant\",\"message\":{\"content\":[\
             {\"type\":\"thinking\",\"thinking\":\"private\",\
             \"signature\":\"sig\"}]}}",
        );
        let ImportedTranscriptContent::Thinking {
            thinking,
            signature,
        } = imported.entries()[0].content()
        else {
            panic!("synthetic entry should be thinking");
        };

        assert!(
            matches!(thinking, ImportedSourceAttestation::Attested(value) if value.as_str() == "private")
        );
        assert!(
            matches!(signature, ImportedSourceAttestation::Attested(value) if value.as_str() == "sig")
        );
    }

    #[test]
    fn s28_inv038_maps_redacted_thinking_data() {
        let imported = convert_synthetic(
            "{\"type\":\"assistant\",\"message\":{\"content\":[\
             {\"type\":\"redacted_thinking\",\"data\":\"sealed\"}]}}",
        );

        assert!(matches!(
            imported.entries()[0].content(),
            ImportedTranscriptContent::RedactedThinking {
                data: ImportedSourceAttestation::Attested(value)
            } if value.as_str() == "sealed"
        ));
    }

    #[test]
    fn s28_inv038_maps_document_media_source_fields_together() {
        let imported = convert_synthetic(
            "{\"type\":\"assistant\",\"message\":{\"content\":[\
             {\"type\":\"document\",\"source\":{\"type\":\"base64\",\
             \"media_type\":\"application/pdf\",\"data\":\"AA==\"}}]}}",
        );
        let ImportedTranscriptContent::Document {
            source: ImportedSourceAttestation::Attested(source),
        } = imported.entries()[0].content()
        else {
            panic!("synthetic entry should be a document");
        };

        assert!(
            matches!(source.kind(), ImportedSourceAttestation::Attested(value) if value.as_str() == "base64")
        );
        assert!(
            matches!(source.media_type(), ImportedSourceAttestation::Attested(value) if value.as_str() == "application/pdf")
        );
        assert!(
            matches!(source.data(), ImportedSourceAttestation::Attested(value) if value.as_str() == "AA==")
        );
    }

    #[test]
    fn s28_inv038_rejects_unpaired_unicode_surrogates() {
        let error = ClaudeCodeJsonlConverter
            .convert(
                conversation(),
                br#"{"type":"user","message":{"content":"\uDEAD"}}"#,
                || ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100)),
            )
            .expect_err("a lone low surrogate has no decoded Unicode scalar");

        assert_eq!(
            error.failure(),
            ClaudeCodeJsonlConversionFailure::InvalidJson { line: 1 }
        );
    }

    #[test]
    fn s28_inv038_preserves_each_distinct_message_absence() {
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
        assert_message_absence(
            &imported.entries()[0],
            ImportedMessageContentAbsence::MessageNotAttested,
        );
        assert_message_absence(
            &imported.entries()[1],
            ImportedMessageContentAbsence::MessageAttestedAbsent,
        );
        assert_message_absence(
            &imported.entries()[2],
            ImportedMessageContentAbsence::ContentNotAttested,
        );
        assert_message_absence(
            &imported.entries()[3],
            ImportedMessageContentAbsence::ContentAttestedAbsent,
        );
        assert_message_absence(
            &imported.entries()[4],
            ImportedMessageContentAbsence::EmptyBlockArray,
        );
    }

    #[test]
    fn s28_inv038_preserves_empty_and_absent_text_attestations() {
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
    }

    #[test]
    fn s28_inv038_preserves_source_only_records() {
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
    fn s28_inv038_unknown_content_block_fails_complete_conversion() {
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
    }

    #[test]
    fn s28_inv038_conversion_error_rendering_is_content_silent() {
        let source = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"secret-before\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[",
            "{\"type\":\"future-secret-kind\",\"payload\":\"secret-after\"}]}}"
        );
        let error = ClaudeCodeJsonlConverter
            .convert(conversation(), source.as_bytes(), || {
                ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100))
            })
            .expect_err("unknown synthetic block must produce a content-silent error");

        let debug = format!("{error:?}");
        assert!(!debug.contains("secret"));
        assert_eq!(error.to_string(), "Claude Code JSONL conversion failed");
    }

    #[test]
    fn s28_inv038_preserves_terminal_lone_carriage_return_as_raw_content() {
        let source = b"{\"type\":\"system\"}\r";
        let imported = ClaudeCodeJsonlConverter
            .convert(conversation(), source, || {
                ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100))
            })
            .expect("a terminal carriage return is valid JSON whitespace");

        assert_eq!(imported.raw_records()[0].bytes(), source);
    }

    #[test]
    fn s28_inv038_terminal_lf_does_not_create_an_empty_record() {
        let imported = ClaudeCodeJsonlConverter
            .convert(conversation(), b"{\"type\":\"system\"}\n", || {
                ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100))
            })
            .expect("a terminal LF is only a delimiter");

        assert_eq!(imported.raw_records().len(), 1);
        assert_eq!(imported.raw_records()[0].bytes(), b"{\"type\":\"system\"}");
    }

    #[test]
    fn s28_inv038_rejects_blank_lines() {
        let error = ClaudeCodeJsonlConverter
            .convert(conversation(), b"{\"type\":\"system\"}\n\n", || {
                ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100))
            })
            .expect_err("blank synthetic JSONL line must fail");
        assert_eq!(
            error.failure(),
            ClaudeCodeJsonlConversionFailure::BlankLine { line: 2 }
        );
    }

    #[test]
    fn s28_inv038_rejects_utf8_bom_at_record_start() {
        let error = ClaudeCodeJsonlConverter
            .convert(conversation(), b"\xef\xbb\xbf{\"type\":\"system\"}", || {
                ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100))
            })
            .expect_err("a UTF-8 BOM is not part of a version-one JSONL record");

        assert_eq!(
            error.failure(),
            ClaudeCodeJsonlConversionFailure::InvalidJson { line: 1 }
        );
    }

    #[test]
    fn s28_inv038_rejects_duplicate_modeled_members() {
        let error = ClaudeCodeJsonlConverter
            .convert(
                conversation(),
                br#"{"type":"user","type":"assistant"}"#,
                || ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100)),
            )
            .expect_err("duplicate modeled members must not select a value");

        assert_eq!(
            error.failure(),
            ClaudeCodeJsonlConversionFailure::InvalidRecordType { line: 1 }
        );
    }

    #[test]
    fn s28_inv038_rejects_message_role_conflicts() {
        let error = ClaudeCodeJsonlConverter
            .convert(
                conversation(),
                br#"{"type":"user","message":{"role":"assistant","content":"x"}}"#,
                || ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100)),
            )
            .expect_err("contradictory synthetic role must fail");
        assert_eq!(
            error.failure(),
            ClaudeCodeJsonlConversionFailure::MessageRoleMismatch { line: 1 }
        );
    }

    #[test]
    fn s28_inv001_inv038_rejects_duplicate_entry_identities() {
        let error = ClaudeCodeJsonlConverter
            .convert(
                conversation(),
                br#"{"type":"user","message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#,
                || ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100)),
            )
            .expect_err("duplicate synthetic entry identities must fail");
        assert!(matches!(
            error.failure(),
            ClaudeCodeJsonlConversionFailure::InvalidAggregate(
                signalbox_domain::ImportedConversationReconstitutionFailure::DuplicateEntry { .. }
            )
        ));
    }
}
