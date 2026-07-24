//! Maximum-fidelity Codex rollout JSONL ingestion.
//!
//! The converter accepts caller-supplied bytes and identities, preserves every
//! raw JSONL record, and emits source-neutral imported entries. It performs no
//! filesystem access and creates no native Signalbox session.

use std::{error::Error, fmt};

use signalbox_application::ImportedConversationConverter;
use signalbox_conversation_import_json::{JsonFailure, parse_record};
use signalbox_domain::{
    ImportedConversation, ImportedConversationFormat, ImportedConversationId,
    ImportedConversationReconstitutionFailure, ImportedMediaSource, ImportedMessageContentAbsence,
    ImportedRawRecordPosition, ImportedRawSourceRecord, ImportedRecordEntryPosition,
    ImportedSourceAttestation, ImportedSourceMetadata, ImportedSpeaker,
    ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText, ImportedToolResultBlock,
    ImportedToolResultValue, ImportedTranscriptContent, ImportedTranscriptEntryId,
    ImportedTranscriptEntryInput, ImportedTranscriptPosition,
};

const FORMAT: ImportedConversationFormat = ImportedConversationFormat::CodexRolloutJsonlV1;

/// Codex rollout JSONL converter version 1.
#[derive(Clone, Copy, Debug, Default)]
pub struct CodexRolloutJsonlConverter;

/// Content-silent reason a complete Codex rollout conversion failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexRolloutJsonlConversionFailure {
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
    /// The top-level rollout item type had an unsupported value shape.
    InvalidRecordType {
        /// One-based physical line number.
        line: u64,
    },
    /// Modeled rollout metadata had an unsupported value shape.
    InvalidSourceMetadata {
        /// One-based physical line number.
        line: u64,
    },
    /// A response-item envelope had an unsupported value shape.
    InvalidResponseItemEnvelope {
        /// One-based physical line number.
        line: u64,
    },
    /// A response-item discriminator had an unsupported value shape.
    InvalidResponseItemType {
        /// One-based physical line number.
        line: u64,
    },
    /// A response message role had an unsupported value shape.
    InvalidMessageRole {
        /// One-based physical line number.
        line: u64,
    },
    /// Response message content had an unsupported value shape.
    InvalidMessageContent {
        /// One-based physical line number.
        line: u64,
    },
    /// One response message content block had an unsupported shape.
    InvalidMessageBlock {
        /// One-based physical line number.
        line: u64,
        /// One-based block position.
        block: u64,
    },
    /// One reasoning item had an unsupported value shape.
    InvalidReasoning {
        /// One-based physical line number.
        line: u64,
    },
    /// One reasoning block had an unsupported shape.
    InvalidReasoningBlock {
        /// One-based physical line number.
        line: u64,
        /// One-based position across summary and content blocks.
        block: u64,
    },
    /// One tool-call item had an unsupported modeled field shape.
    InvalidToolCall {
        /// One-based physical line number.
        line: u64,
    },
    /// One tool-result item had an unsupported modeled field shape.
    InvalidToolResult {
        /// One-based physical line number.
        line: u64,
    },
    /// One structured tool-result block had an unsupported shape.
    InvalidToolResultBlock {
        /// One-based physical line number.
        line: u64,
        /// One-based result-block position.
        block: u64,
    },
    /// A required source or entry position could not be represented.
    PositionExhausted,
    /// The converted candidate violated an imported-conversation invariant.
    InvalidAggregate(ImportedConversationReconstitutionFailure),
}

/// A complete Codex rollout JSONL conversion failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodexRolloutJsonlConversionError {
    failure: CodexRolloutJsonlConversionFailure,
}

impl CodexRolloutJsonlConversionError {
    /// Returns the content-silent failure classification.
    pub const fn failure(self) -> CodexRolloutJsonlConversionFailure {
        self.failure
    }
}

impl fmt::Display for CodexRolloutJsonlConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Codex rollout JSONL conversion failed")
    }
}

impl Error for CodexRolloutJsonlConversionError {}

impl ImportedConversationConverter for CodexRolloutJsonlConverter {
    type Error = CodexRolloutJsonlConversionError;

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
            |error| CodexRolloutJsonlConversionError {
                failure: CodexRolloutJsonlConversionFailure::InvalidAggregate(error.failure()),
            },
        )
    }
}

fn position_error() -> CodexRolloutJsonlConversionError {
    conversion_error(CodexRolloutJsonlConversionFailure::PositionExhausted)
}

fn ordinal(index: usize) -> Result<u64, CodexRolloutJsonlConversionError> {
    u64::try_from(index)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(position_error)
}

fn json_error(line: u64, failure: JsonFailure) -> CodexRolloutJsonlConversionError {
    let failure = match failure {
        JsonFailure::InvalidUtf8 => CodexRolloutJsonlConversionFailure::InvalidUtf8 { line },
        JsonFailure::Syntax => CodexRolloutJsonlConversionFailure::InvalidJson { line },
        JsonFailure::DepthExceeded => {
            CodexRolloutJsonlConversionFailure::JsonDepthExceeded { line }
        }
    };
    conversion_error(failure)
}

fn conversion_error(
    failure: CodexRolloutJsonlConversionFailure,
) -> CodexRolloutJsonlConversionError {
    CodexRolloutJsonlConversionError { failure }
}

fn split_records(source: &[u8]) -> Result<Vec<&[u8]>, CodexRolloutJsonlConversionError> {
    if source.is_empty() {
        return Err(conversion_error(
            CodexRolloutJsonlConversionFailure::EmptySource,
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
                CodexRolloutJsonlConversionFailure::BlankLine {
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
) -> Result<Vec<PendingEntry>, CodexRolloutJsonlConversionError> {
    let ImportedStructuredValue::Object(record) = normalized else {
        return Err(conversion_error(
            CodexRolloutJsonlConversionFailure::TopLevelNotObject { line },
        ));
    };
    let source_type = text_attestation(record, "type").map_err(|()| {
        conversion_error(CodexRolloutJsonlConversionFailure::InvalidRecordType { line })
    })?;
    if matches!(
        &source_type,
        ImportedSourceAttestation::Attested(value) if value.as_str() == "response_item"
    ) {
        normalize_response_item(record, source_type, line)
    } else {
        source_event(
            record,
            source_type,
            ImportedSourceAttestation::NotAttested,
            line,
        )
        .map(|entry| vec![entry])
    }
}

fn normalize_response_item(
    record: &[ImportedStructuredObjectMember],
    source_type: ImportedSourceAttestation<ImportedText>,
    line: u64,
) -> Result<Vec<PendingEntry>, CodexRolloutJsonlConversionError> {
    let payload = match unique_field(record, "payload").map_err(|()| invalid_response(line))? {
        Some(ImportedStructuredValue::Object(payload)) => payload,
        None | Some(ImportedStructuredValue::Null) | Some(_) => {
            return Err(invalid_response(line));
        }
    };
    let payload_type = text_attestation(payload, "type").map_err(|()| {
        conversion_error(CodexRolloutJsonlConversionFailure::InvalidResponseItemType { line })
    })?;
    match &payload_type {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "message" => {
            normalize_message(record, payload, source_type, line)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "reasoning" => {
            normalize_reasoning(record, payload, source_type, line)
        }
        ImportedSourceAttestation::Attested(value)
            if matches!(value.as_str(), "function_call" | "custom_tool_call") =>
        {
            normalize_named_tool_call(record, payload, line)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "tool_search_call" => {
            normalize_structured_tool_call(record, payload, "arguments", line)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "local_shell_call" => {
            normalize_structured_tool_call(record, payload, "action", line)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "web_search_call" => {
            normalize_web_search_call(record, payload, line)
        }
        ImportedSourceAttestation::Attested(value)
            if matches!(
                value.as_str(),
                "function_call_output" | "custom_tool_call_output"
            ) =>
        {
            normalize_tool_result(record, payload, line)
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "tool_search_output" => {
            normalize_tool_search_result(record, payload, line)
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => source_event_with_payload(
            record,
            payload,
            source_type,
            ImportedSourceAttestation::NotAttested,
            line,
        )
        .map(|entry| vec![entry]),
    }
}

fn normalize_message(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    source_type: ImportedSourceAttestation<ImportedText>,
    line: u64,
) -> Result<Vec<PendingEntry>, CodexRolloutJsonlConversionError> {
    let role = text_attestation(payload, "role").map_err(|()| {
        conversion_error(CodexRolloutJsonlConversionFailure::InvalidMessageRole { line })
    })?;
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
        return source_event_with_payload(
            record,
            payload,
            source_type,
            role_for_metadata(&role),
            line,
        )
        .map(|entry| vec![entry]);
    };
    let content = match unique_field(payload, "content").map_err(|()| invalid_message(line))? {
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
            .enumerate()
            .map(|(index, block)| normalize_message_block(block, line, ordinal(index)?))
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(invalid_message(line)),
    };
    let source = source_metadata(
        record,
        Some(payload),
        ImportedSourceAttestation::Attested(speaker),
        line,
    )?;
    Ok(content
        .into_iter()
        .map(|content| PendingEntry {
            source_speaker: ImportedSourceAttestation::Attested(speaker),
            content,
            source: source.clone(),
        })
        .collect())
}

fn role_for_metadata(
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

fn normalize_message_block(
    value: &ImportedStructuredValue,
    line: u64,
    block: u64,
) -> Result<ImportedTranscriptContent, CodexRolloutJsonlConversionError> {
    let ImportedStructuredValue::Object(members) = value else {
        return Err(invalid_message_block(line, block));
    };
    let source_type =
        text_attestation(members, "type").map_err(|()| invalid_message_block(line, block))?;
    match &source_type {
        ImportedSourceAttestation::Attested(value)
            if matches!(value.as_str(), "input_text" | "output_text") =>
        {
            Ok(ImportedTranscriptContent::Text(
                text_attestation(members, "text")
                    .map_err(|()| invalid_message_block(line, block))?,
            ))
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => {
            Ok(ImportedTranscriptContent::SourceMessageBlock { source_type })
        }
    }
}

fn normalize_reasoning(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    source_type: ImportedSourceAttestation<ImportedText>,
    line: u64,
) -> Result<Vec<PendingEntry>, CodexRolloutJsonlConversionError> {
    let mut content = Vec::new();
    append_reasoning_blocks(payload, "summary", line, &mut content)?;
    append_reasoning_blocks(payload, "content", line, &mut content)?;
    match unique_field(payload, "encrypted_content").map_err(|()| invalid_reasoning(line))? {
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
        Some(_) => return Err(invalid_reasoning(line)),
    }
    if content.is_empty() {
        return source_event_with_payload(
            record,
            payload,
            source_type,
            ImportedSourceAttestation::NotAttested,
            line,
        )
        .map(|entry| vec![entry]);
    }
    let source = source_metadata(
        record,
        Some(payload),
        ImportedSourceAttestation::NotAttested,
        line,
    )?;
    Ok(content
        .into_iter()
        .map(|content| PendingEntry {
            source_speaker: ImportedSourceAttestation::NotAttested,
            content,
            source: source.clone(),
        })
        .collect())
}

fn append_reasoning_blocks(
    payload: &[ImportedStructuredObjectMember],
    field: &str,
    line: u64,
    content: &mut Vec<ImportedTranscriptContent>,
) -> Result<(), CodexRolloutJsonlConversionError> {
    let Some(value) = unique_field(payload, field).map_err(|()| invalid_reasoning(line))? else {
        return Ok(());
    };
    let blocks = match value {
        ImportedStructuredValue::Null => return Ok(()),
        ImportedStructuredValue::Array(blocks) => blocks,
        _ => return Err(invalid_reasoning(line)),
    };
    for block in blocks {
        let block_position = ordinal(content.len())?;
        let ImportedStructuredValue::Object(members) = block else {
            return Err(invalid_reasoning_block(line, block_position));
        };
        let source_type = text_attestation(members, "type")
            .map_err(|()| invalid_reasoning_block(line, block_position))?;
        let normalized = match &source_type {
            ImportedSourceAttestation::Attested(value)
                if matches!(value.as_str(), "summary_text" | "reasoning_text" | "text") =>
            {
                ImportedTranscriptContent::Thinking {
                    thinking: text_attestation(members, "text")
                        .map_err(|()| invalid_reasoning_block(line, block_position))?,
                    signature: ImportedSourceAttestation::NotAttested,
                }
            }
            ImportedSourceAttestation::Attested(_)
            | ImportedSourceAttestation::AttestedAbsent
            | ImportedSourceAttestation::NotAttested => {
                ImportedTranscriptContent::SourceMessageBlock { source_type }
            }
        };
        content.push(normalized);
    }
    Ok(())
}

fn normalize_named_tool_call(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    line: u64,
) -> Result<Vec<PendingEntry>, CodexRolloutJsonlConversionError> {
    let payload_type = text_attestation(payload, "type").map_err(|()| invalid_tool_call(line))?;
    let input_field = match &payload_type {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "function_call" => {
            "arguments"
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "custom_tool_call" => {
            "input"
        }
        _ => return Err(invalid_tool_call(line)),
    };
    let content = ImportedTranscriptContent::ToolCall {
        source_call_id: text_attestation(payload, "call_id")
            .map_err(|()| invalid_tool_call(line))?,
        name: text_attestation(payload, "name").map_err(|()| invalid_tool_call(line))?,
        input: structured_attestation(payload, input_field)
            .map_err(|()| invalid_tool_call(line))?,
        caller: ImportedSourceAttestation::NotAttested,
    };
    typed_response_entry(record, payload, content, line).map(|entry| vec![entry])
}

fn normalize_structured_tool_call(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    input_field: &str,
    line: u64,
) -> Result<Vec<PendingEntry>, CodexRolloutJsonlConversionError> {
    let content = ImportedTranscriptContent::ToolCall {
        source_call_id: text_attestation(payload, "call_id")
            .map_err(|()| invalid_tool_call(line))?,
        name: ImportedSourceAttestation::NotAttested,
        input: structured_attestation(payload, input_field)
            .map_err(|()| invalid_tool_call(line))?,
        caller: ImportedSourceAttestation::NotAttested,
    };
    typed_response_entry(record, payload, content, line).map(|entry| vec![entry])
}

fn normalize_web_search_call(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    line: u64,
) -> Result<Vec<PendingEntry>, CodexRolloutJsonlConversionError> {
    let content = ImportedTranscriptContent::ToolCall {
        source_call_id: text_attestation(payload, "id").map_err(|()| invalid_tool_call(line))?,
        name: ImportedSourceAttestation::NotAttested,
        input: structured_attestation(payload, "action").map_err(|()| invalid_tool_call(line))?,
        caller: ImportedSourceAttestation::NotAttested,
    };
    typed_response_entry(record, payload, content, line).map(|entry| vec![entry])
}

fn normalize_tool_result(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    line: u64,
) -> Result<Vec<PendingEntry>, CodexRolloutJsonlConversionError> {
    let content = match unique_field(payload, "output").map_err(|()| invalid_tool_result(line))? {
        None => ImportedSourceAttestation::NotAttested,
        Some(ImportedStructuredValue::Null) => ImportedSourceAttestation::AttestedAbsent,
        Some(ImportedStructuredValue::String(value)) => {
            ImportedSourceAttestation::Attested(ImportedToolResultValue::Text(value.clone()))
        }
        Some(ImportedStructuredValue::Array(blocks)) => {
            let blocks = blocks
                .iter()
                .enumerate()
                .map(|(index, block)| normalize_tool_result_block(block, line, ordinal(index)?))
                .collect::<Result<Vec<_>, _>>()?;
            ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                blocks.into_boxed_slice(),
            ))
        }
        Some(_) => return Err(invalid_tool_result(line)),
    };
    let content = ImportedTranscriptContent::ToolResult {
        source_call_id: text_attestation(payload, "call_id")
            .map_err(|()| invalid_tool_result(line))?,
        content,
        is_error: ImportedSourceAttestation::NotAttested,
    };
    typed_response_entry(record, payload, content, line).map(|entry| vec![entry])
}

fn normalize_tool_result_block(
    value: &ImportedStructuredValue,
    line: u64,
    block: u64,
) -> Result<ImportedToolResultBlock, CodexRolloutJsonlConversionError> {
    let ImportedStructuredValue::Object(members) = value else {
        return Err(invalid_tool_result_block(line, block));
    };
    let source_type =
        text_attestation(members, "type").map_err(|()| invalid_tool_result_block(line, block))?;
    match &source_type {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "input_text" => {
            Ok(ImportedToolResultBlock::Text(
                text_attestation(members, "text")
                    .map_err(|()| invalid_tool_result_block(line, block))?,
            ))
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "input_image" => {
            Ok(ImportedToolResultBlock::Image(
                media_attestation(members, source_type.clone())
                    .map_err(|()| invalid_tool_result_block(line, block))?,
            ))
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => {
            Ok(ImportedToolResultBlock::SourceResultBlock { source_type })
        }
    }
}

fn normalize_tool_search_result(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    line: u64,
) -> Result<Vec<PendingEntry>, CodexRolloutJsonlConversionError> {
    let content = match unique_field(payload, "tools").map_err(|()| invalid_tool_result(line))? {
        None => ImportedSourceAttestation::NotAttested,
        Some(ImportedStructuredValue::Null) => ImportedSourceAttestation::AttestedAbsent,
        Some(ImportedStructuredValue::Array(tools)) => {
            let blocks = tools
                .iter()
                .map(|tool| {
                    let source_type = match tool {
                        ImportedStructuredValue::Object(members) => {
                            text_attestation(members, "type")
                                .map_err(|()| invalid_tool_result(line))?
                        }
                        _ => ImportedSourceAttestation::NotAttested,
                    };
                    Ok(ImportedToolResultBlock::SourceResultBlock { source_type })
                })
                .collect::<Result<Vec<_>, CodexRolloutJsonlConversionError>>()?;
            ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                blocks.into_boxed_slice(),
            ))
        }
        Some(_) => return Err(invalid_tool_result(line)),
    };
    let content = ImportedTranscriptContent::ToolResult {
        source_call_id: text_attestation(payload, "call_id")
            .map_err(|()| invalid_tool_result(line))?,
        content,
        is_error: ImportedSourceAttestation::NotAttested,
    };
    typed_response_entry(record, payload, content, line).map(|entry| vec![entry])
}

fn typed_response_entry(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    content: ImportedTranscriptContent,
    line: u64,
) -> Result<PendingEntry, CodexRolloutJsonlConversionError> {
    Ok(PendingEntry {
        source_speaker: ImportedSourceAttestation::NotAttested,
        content,
        source: source_metadata(
            record,
            Some(payload),
            ImportedSourceAttestation::NotAttested,
            line,
        )?,
    })
}

fn source_event(
    record: &[ImportedStructuredObjectMember],
    source_type: ImportedSourceAttestation<ImportedText>,
    message_role: ImportedSourceAttestation<ImportedSpeaker>,
    line: u64,
) -> Result<PendingEntry, CodexRolloutJsonlConversionError> {
    let payload = optional_object(record, "payload").map_err(|()| {
        conversion_error(CodexRolloutJsonlConversionFailure::InvalidSourceMetadata { line })
    })?;
    source_event_with_optional_payload(record, payload, source_type, message_role, line)
}

fn source_event_with_payload(
    record: &[ImportedStructuredObjectMember],
    payload: &[ImportedStructuredObjectMember],
    source_type: ImportedSourceAttestation<ImportedText>,
    message_role: ImportedSourceAttestation<ImportedSpeaker>,
    line: u64,
) -> Result<PendingEntry, CodexRolloutJsonlConversionError> {
    source_event_with_optional_payload(record, Some(payload), source_type, message_role, line)
}

fn source_event_with_optional_payload(
    record: &[ImportedStructuredObjectMember],
    payload: Option<&[ImportedStructuredObjectMember]>,
    source_type: ImportedSourceAttestation<ImportedText>,
    message_role: ImportedSourceAttestation<ImportedSpeaker>,
    line: u64,
) -> Result<PendingEntry, CodexRolloutJsonlConversionError> {
    Ok(PendingEntry {
        source_speaker: ImportedSourceAttestation::NotAttested,
        content: ImportedTranscriptContent::SourceEvent { source_type },
        source: source_metadata(record, payload, message_role, line)?,
    })
}

fn source_metadata(
    record: &[ImportedStructuredObjectMember],
    payload: Option<&[ImportedStructuredObjectMember]>,
    message_role: ImportedSourceAttestation<ImportedSpeaker>,
    line: u64,
) -> Result<ImportedSourceMetadata, CodexRolloutJsonlConversionError> {
    let invalid =
        |()| conversion_error(CodexRolloutJsonlConversionFailure::InvalidSourceMetadata { line });
    let record_id = payload
        .map(|payload| text_attestation(payload, "id"))
        .transpose()
        .map_err(invalid)?
        .unwrap_or(ImportedSourceAttestation::NotAttested);
    let source_session_id = payload
        .map(|payload| text_attestation(payload, "session_id"))
        .transpose()
        .map_err(invalid)?
        .unwrap_or(ImportedSourceAttestation::NotAttested);
    Ok(ImportedSourceMetadata::new(
        record_id,
        ImportedSourceAttestation::NotAttested,
        source_session_id,
        text_attestation(record, "timestamp").map_err(invalid)?,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        message_role,
    ))
}

fn optional_object<'members>(
    members: &'members [ImportedStructuredObjectMember],
    name: &str,
) -> Result<Option<&'members [ImportedStructuredObjectMember]>, ()> {
    match unique_field(members, name)? {
        Some(ImportedStructuredValue::Object(value)) => Ok(Some(value)),
        None | Some(ImportedStructuredValue::Null) | Some(_) => Ok(None),
    }
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

fn media_attestation(
    members: &[ImportedStructuredObjectMember],
    source_type: ImportedSourceAttestation<ImportedText>,
) -> Result<ImportedSourceAttestation<ImportedMediaSource>, ()> {
    let image_url = text_attestation(members, "image_url")?;
    Ok(ImportedSourceAttestation::Attested(
        ImportedMediaSource::new(
            source_type,
            ImportedSourceAttestation::NotAttested,
            image_url,
        ),
    ))
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

fn invalid_response(line: u64) -> CodexRolloutJsonlConversionError {
    conversion_error(CodexRolloutJsonlConversionFailure::InvalidResponseItemEnvelope { line })
}

fn invalid_message(line: u64) -> CodexRolloutJsonlConversionError {
    conversion_error(CodexRolloutJsonlConversionFailure::InvalidMessageContent { line })
}

fn invalid_message_block(line: u64, block: u64) -> CodexRolloutJsonlConversionError {
    conversion_error(CodexRolloutJsonlConversionFailure::InvalidMessageBlock { line, block })
}

fn invalid_reasoning(line: u64) -> CodexRolloutJsonlConversionError {
    conversion_error(CodexRolloutJsonlConversionFailure::InvalidReasoning { line })
}

fn invalid_reasoning_block(line: u64, block: u64) -> CodexRolloutJsonlConversionError {
    conversion_error(CodexRolloutJsonlConversionFailure::InvalidReasoningBlock { line, block })
}

fn invalid_tool_call(line: u64) -> CodexRolloutJsonlConversionError {
    conversion_error(CodexRolloutJsonlConversionFailure::InvalidToolCall { line })
}

fn invalid_tool_result(line: u64) -> CodexRolloutJsonlConversionError {
    conversion_error(CodexRolloutJsonlConversionFailure::InvalidToolResult { line })
}

fn invalid_tool_result_block(line: u64, block: u64) -> CodexRolloutJsonlConversionError {
    conversion_error(CodexRolloutJsonlConversionFailure::InvalidToolResultBlock { line, block })
}

#[cfg(test)]
mod tests {
    use signalbox_application::ImportedConversationConverter;
    use signalbox_domain::{
        ImportedConversation, ImportedConversationFormat, ImportedConversationId,
        ImportedMessageContentAbsence, ImportedSourceAttestation, ImportedSpeaker,
        ImportedStructuredValue, ImportedToolResultBlock, ImportedToolResultValue,
        ImportedTranscriptContent, ImportedTranscriptEntryId,
    };
    use uuid::Uuid;

    use super::{CodexRolloutJsonlConversionFailure, CodexRolloutJsonlConverter};

    fn conversation() -> ImportedConversationId {
        ImportedConversationId::from_uuid(Uuid::from_u128(1))
    }

    #[track_caller]
    fn convert_synthetic(source: &str) -> ImportedConversation {
        let mut next_identity = 100_u128;
        CodexRolloutJsonlConverter
            .convert(conversation(), source.as_bytes(), || {
                let identity = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(next_identity));
                next_identity = next_identity
                    .checked_add(1)
                    .expect("synthetic identity range is bounded");
                identity
            })
            .unwrap_or_else(|error| {
                panic!("synthetic rollout should convert: {:?}", error.failure())
            })
    }

    #[test]
    fn s28_inv038_converter_declares_codex_rollout_version_one() {
        assert_eq!(
            CodexRolloutJsonlConverter.format(),
            ImportedConversationFormat::CodexRolloutJsonlV1
        );
    }

    #[test]
    fn s28_inv038_normalizes_complete_rollout_vocabulary() {
        let imported = convert_synthetic(
            "{\"timestamp\":\"t0\",\"type\":\"session_meta\",\"payload\":{\"id\":\"session-item\",\"session_id\":\"session\"}}\n\
             {\"timestamp\":\"t1\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"question\"},{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,AA\",\"detail\":\"high\"}]}}\n\
             {\"timestamp\":\"t2\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n\
             {\"timestamp\":\"t3\",\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"summary\"}],\"content\":[{\"type\":\"reasoning_text\",\"text\":\"detail\"}],\"encrypted_content\":\"opaque\"}}\n\
             {\"timestamp\":\"t4\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"read_item\",\"arguments\":\"{\\\"path\\\":\\\"synthetic\\\"}\"}}\n\
             {\"timestamp\":\"t5\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call-1\",\"output\":[{\"type\":\"input_text\",\"text\":\"result\"},{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,BB\"},{\"type\":\"future_result\",\"value\":1}]}}\n\
             {\"timestamp\":\"t6\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"mirrored\"}}",
        );

        assert_eq!(imported.raw_records().len(), 7);
        assert_eq!(imported.entries().len(), 10);
        assert_eq!(imported.frontiers().count(), imported.entries().len());
        assert!(matches!(
            imported.entries()[0].content(),
            ImportedTranscriptContent::SourceEvent { .. }
        ));
        assert_eq!(
            imported.entries()[1].source_speaker(),
            &ImportedSourceAttestation::Attested(ImportedSpeaker::User)
        );
        assert!(matches!(
            imported.entries()[1].content(),
            ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text))
                if text.as_str() == "question"
        ));
        assert!(matches!(
            imported.entries()[2].content(),
            ImportedTranscriptContent::SourceMessageBlock {
                source_type: ImportedSourceAttestation::Attested(source_type),
            } if source_type.as_str() == "input_image"
        ));
        assert_eq!(
            imported.entries()[3].source_speaker(),
            &ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant)
        );
        assert!(matches!(
            imported.entries()[4].content(),
            ImportedTranscriptContent::Thinking {
                thinking: ImportedSourceAttestation::Attested(text),
                signature: ImportedSourceAttestation::NotAttested,
            } if text.as_str() == "summary"
        ));
        assert!(matches!(
            imported.entries()[6].content(),
            ImportedTranscriptContent::RedactedThinking {
                data: ImportedSourceAttestation::Attested(data),
            } if data.as_str() == "opaque"
        ));
        assert!(matches!(
            imported.entries()[7].content(),
            ImportedTranscriptContent::ToolCall {
                source_call_id: ImportedSourceAttestation::Attested(call_id),
                name: ImportedSourceAttestation::Attested(name),
                input: ImportedSourceAttestation::Attested(
                    ImportedStructuredValue::String(arguments),
                ),
                caller: ImportedSourceAttestation::NotAttested,
            } if call_id.as_str() == "call-1"
                && name.as_str() == "read_item"
                && arguments.as_str() == "{\"path\":\"synthetic\"}"
        ));
        let ImportedTranscriptContent::ToolResult { content, .. } = imported.entries()[8].content()
        else {
            panic!("synthetic output should normalize as a tool result");
        };
        let ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(blocks)) = content
        else {
            panic!("synthetic output should retain ordered result blocks");
        };
        assert!(matches!(
            &blocks[0],
            ImportedToolResultBlock::Text(ImportedSourceAttestation::Attested(text))
                if text.as_str() == "result"
        ));
        assert!(matches!(
            &blocks[1],
            ImportedToolResultBlock::Image(ImportedSourceAttestation::Attested(source))
                if matches!(
                    source.data(),
                    ImportedSourceAttestation::Attested(data)
                        if data.as_str() == "data:image/png;base64,BB"
                )
        ));
        assert!(matches!(
            &blocks[2],
            ImportedToolResultBlock::SourceResultBlock {
                source_type: ImportedSourceAttestation::Attested(source_type),
            } if source_type.as_str() == "future_result"
        ));
        assert!(matches!(
            imported.entries()[9].content(),
            ImportedTranscriptContent::SourceEvent { .. }
        ));
    }

    #[test]
    fn s28_inv038_event_mirror_and_developer_message_remain_source_events() {
        let imported = convert_synthetic(
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"developer\",\"content\":[{\"type\":\"input_text\",\"text\":\"context\"}]}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"mirrored\"}}",
        );

        assert_eq!(imported.entries().len(), 2);
        assert!(imported.entries().iter().all(|entry| matches!(
            entry.content(),
            ImportedTranscriptContent::SourceEvent { .. }
        )));
        assert!(imported.entries().iter().all(|entry| matches!(
            entry.source_speaker(),
            ImportedSourceAttestation::NotAttested
        )));
    }

    #[test]
    fn s28_inv038_preserves_typed_absence_without_placeholders() {
        let imported = convert_synthetic(
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\"}}\n\
             {\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":null,\"output\":null}}\n\
             {\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"summary\":[],\"content\":null,\"encrypted_content\":null}}",
        );

        assert_eq!(
            imported.entries()[0].content(),
            &ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::ContentNotAttested,
            )
        );
        assert!(matches!(
            imported.entries()[1].content(),
            ImportedTranscriptContent::ToolResult {
                source_call_id: ImportedSourceAttestation::AttestedAbsent,
                content: ImportedSourceAttestation::AttestedAbsent,
                is_error: ImportedSourceAttestation::NotAttested,
            }
        ));
        assert!(matches!(
            imported.entries()[2].content(),
            ImportedTranscriptContent::RedactedThinking {
                data: ImportedSourceAttestation::AttestedAbsent,
            }
        ));
    }

    #[test]
    fn s28_inv038_crlf_delimiter_is_not_raw_record_content() {
        let imported = convert_synthetic(
            "{\"type\":\"session_meta\",\"payload\":{}}\r\n{\"type\":\"event_msg\",\"payload\":{}}",
        );

        assert_eq!(
            imported.raw_records()[0].bytes(),
            br#"{"type":"session_meta","payload":{}}"#
        );
    }

    #[test]
    fn s28_inv038_rejects_duplicate_consulted_members_content_silently() {
        let error = CodexRolloutJsonlConverter
            .convert(
                conversation(),
                br#"{"type":"response_item","payload":{"type":"message","role":"user","role":"assistant","content":[]}}"#,
                || ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(100)),
            )
            .expect_err("duplicate synthetic role must fail");

        assert_eq!(
            error.failure(),
            CodexRolloutJsonlConversionFailure::InvalidMessageRole { line: 1 }
        );
        assert_eq!(error.to_string(), "Codex rollout JSONL conversion failed");
    }
}
