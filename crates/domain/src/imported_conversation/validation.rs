//! Imported conversation reconstitution validation for `docs/spec/conversation-import.md`.

use super::content::ImportedSpeaker;
use super::content::ImportedTranscriptContent;
use super::conversation::ImportedConversation;
use super::digest::ImportedConversationSourceDigest;
use super::digest::ImportedRawRecordConversionDigest;
use super::digest::ImportedRawRecordHash;
use super::format::ImportedConversationFormat;
use super::position::ImportedRawRecordPosition;
use super::position::ImportedRecordEntryPosition;
use super::position::ImportedTranscriptPosition;
use super::projection::projected_entries;
use super::reconstitution::ImportedConversationReconstitutionError;
use super::reconstitution::ImportedConversationReconstitutionFailure;
use super::reconstitution::ImportedConversationReconstitutionInput;
use super::record::ImportedRawSourceRecordReconstitutionInput;
use super::record::ImportedTranscriptEntryInput;
use super::structured_field::imported_text_attestation;
use super::structured_field::projected_text_attestation;
use super::structured_field::unique_structured_field;
use super::structured_value::ImportedSourceAttestation;
use super::structured_value::ImportedStructuredObjectMember;
use super::structured_value::ImportedStructuredValue;
use super::structured_value::ImportedText;
use crate::ImportedConversationId;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

const MAX_STRUCTURED_CONTAINER_DEPTH: usize = 128;
/// Yields, for every raw record in physical order whose normalized value is
/// an object whose first `type` member is the exact `record_type` string, the
/// string at the record's `path` of first-member lookups.
///
/// Each path step selects the first member with that exact name; a step whose
/// member is absent, or whose value is not the shape the next step needs,
/// exhausts that record without failing the derivation.
pub(super) fn typed_record_string_candidates<'conversation>(
    conversation: &'conversation ImportedConversation,
    record_type: &'conversation str,
    path: &'conversation [&'conversation str],
) -> impl Iterator<Item = &'conversation str> {
    conversation.raw_records().iter().filter_map(move |record| {
        let ImportedStructuredValue::Object(members) = record.normalized() else {
            return None;
        };
        let ImportedStructuredValue::String(found_type) = first_member(members, "type")? else {
            return None;
        };
        if found_type.as_str() != record_type {
            return None;
        }
        let mut value = record.normalized();
        for step in path {
            let ImportedStructuredValue::Object(members) = value else {
                return None;
            };
            value = first_member(members, step)?;
        }
        let ImportedStructuredValue::String(text) = value else {
            return None;
        };
        Some(text.as_str())
    })
}

/// Yields every attested-text entry with an attested user speaker, in
/// imported order.
pub(super) fn attested_user_text_candidates(
    conversation: &ImportedConversation,
) -> impl Iterator<Item = &str> {
    conversation.entries().iter().filter_map(|entry| {
        if *entry.source_speaker() != ImportedSourceAttestation::Attested(ImportedSpeaker::User) {
            return None;
        }
        let ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text)) =
            entry.content()
        else {
            return None;
        };
        Some(text.as_str())
    })
}

/// Selects the first object member with the exact name, retaining duplicates
/// as inert later members.
fn first_member<'members>(
    members: &'members [ImportedStructuredObjectMember],
    name: &str,
) -> Option<&'members ImportedStructuredValue> {
    members
        .iter()
        .find(|member| member.name().as_str() == name)
        .map(ImportedStructuredObjectMember::value)
}

pub(super) fn conversion_error(
    id: ImportedConversationId,
    format: ImportedConversationFormat,
    raw_records: Vec<ImportedRawSourceRecordReconstitutionInput>,
    entries: Vec<ImportedTranscriptEntryInput>,
    failure: ImportedConversationReconstitutionFailure,
) -> ImportedConversationReconstitutionError {
    let stored_source_digest = ImportedConversationSourceDigest::derive(format, &raw_records);
    ImportedConversationReconstitutionError {
        input: Box::new(ImportedConversationReconstitutionInput::new(
            id,
            id,
            format,
            stored_source_digest,
            u64::try_from(raw_records.len()).unwrap_or(u64::MAX),
            raw_records,
            u64::try_from(entries.len()).unwrap_or(u64::MAX),
            entries,
        )),
        failure,
    }
}

pub(super) fn validate_reconstitution(
    input: &ImportedConversationReconstitutionInput,
) -> Result<(), ImportedConversationReconstitutionFailure> {
    if input.requested_conversation != input.stored_conversation {
        return Err(ImportedConversationReconstitutionFailure::RequestedConversationMismatch);
    }
    validate_raw_records(input)?;
    validate_entries(input)
}

fn validate_raw_records(
    input: &ImportedConversationReconstitutionInput,
) -> Result<(), ImportedConversationReconstitutionFailure> {
    if input.raw_records.is_empty() {
        return Err(ImportedConversationReconstitutionFailure::EmptyRawRecords);
    }
    if u64::try_from(input.raw_records.len()).ok() != Some(input.declared_raw_record_count) {
        return Err(
            ImportedConversationReconstitutionFailure::DeclaredRawRecordCountMismatch {
                declared: input.declared_raw_record_count,
                actual: input.raw_records.len(),
            },
        );
    }
    let mut expected = ImportedRawRecordPosition::first();
    let mut bytes_by_hash = BTreeMap::new();
    for (index, record) in input.raw_records.iter().enumerate() {
        if record.position != expected {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordPositionMismatch {
                    expected,
                    actual: record.position,
                },
            );
        }
        if ImportedRawRecordHash::digest(&record.bytes) != record.stored_hash {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordHashMismatch {
                    position: record.position,
                },
            );
        }
        if record.bytes.is_empty() {
            return Err(ImportedConversationReconstitutionFailure::EmptyRawRecord {
                position: record.position,
            });
        }
        if let Some(existing_bytes) = bytes_by_hash.insert(record.stored_hash, &record.bytes)
            && existing_bytes != &record.bytes
        {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordHashCollision {
                    position: record.position,
                },
            );
        }
        if !matches!(&record.normalized, ImportedStructuredValue::Object(_)) {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordNormalizedValueNotObject {
                    position: record.position,
                },
            );
        }
        if !structured_value_within_depth(&record.normalized) {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordStructuredValueDepthExceeded {
                    position: record.position,
                },
            );
        }
        if record.stored_conversion_digest
            != ImportedRawRecordConversionDigest::derive(record.stored_hash, &record.normalized)
        {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordConversionDigestMismatch {
                    position: record.position,
                },
            );
        }
        if index + 1 < input.raw_records.len() {
            expected = expected
                .checked_next()
                .ok_or(ImportedConversationReconstitutionFailure::PositionExhausted)?;
        }
    }
    let expected_digest =
        ImportedConversationSourceDigest::derive(input.format, &input.raw_records);
    if input.stored_source_digest != expected_digest {
        return Err(
            ImportedConversationReconstitutionFailure::SourceDigestMismatch {
                expected: expected_digest,
                actual: input.stored_source_digest,
            },
        );
    }
    Ok(())
}

fn validate_entries(
    input: &ImportedConversationReconstitutionInput,
) -> Result<(), ImportedConversationReconstitutionFailure> {
    if input.entries.is_empty() {
        return Err(ImportedConversationReconstitutionFailure::EmptyEntries);
    }
    if u64::try_from(input.entries.len()).ok() != Some(input.declared_entry_count) {
        return Err(
            ImportedConversationReconstitutionFailure::DeclaredEntryCountMismatch {
                declared: input.declared_entry_count,
                actual: input.entries.len(),
            },
        );
    }

    let mut expected_position = ImportedTranscriptPosition::first();
    let mut expected_raw_position = ImportedRawRecordPosition::first();
    let mut expected_within_position = ImportedRecordEntryPosition::first();
    let mut identities = BTreeSet::new();
    let last_raw_position = input
        .raw_records
        .last()
        .map(ImportedRawSourceRecordReconstitutionInput::position)
        .ok_or(ImportedConversationReconstitutionFailure::EmptyRawRecords)?;
    let first_raw = input
        .raw_records
        .first()
        .ok_or(ImportedConversationReconstitutionFailure::EmptyRawRecords)?;
    let mut projected_raw_position = first_raw.position;
    let mut expected_entries =
        projected_entries(input.format, first_raw.normalized()).map_err(|()| {
            ImportedConversationReconstitutionFailure::RawRecordProjectionInvalid {
                position: first_raw.position,
            }
        })?;
    let mut projected_entry_index = 0_usize;
    for (index, entry) in input.entries.iter().enumerate() {
        if entry.conversation != input.stored_conversation {
            return Err(
                ImportedConversationReconstitutionFailure::EntryConversationMismatch {
                    entry: entry.identity,
                },
            );
        }
        if entry.position != expected_position {
            return Err(
                ImportedConversationReconstitutionFailure::EntryPositionMismatch {
                    entry: entry.identity,
                    expected: expected_position,
                    actual: entry.position,
                },
            );
        }
        if !identities.insert(entry.identity) {
            return Err(ImportedConversationReconstitutionFailure::DuplicateEntry {
                entry: entry.identity,
            });
        }
        if entry.raw_record_position > last_raw_position {
            return Err(
                ImportedConversationReconstitutionFailure::EntryRawRecordNotFound {
                    entry: entry.identity,
                    position: entry.raw_record_position,
                },
            );
        }
        if index == 0 && entry.raw_record_position != expected_raw_position {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordWithoutEntry {
                    position: expected_raw_position,
                },
            );
        }

        if entry.raw_record_position != expected_raw_position {
            let next_raw = expected_raw_position.checked_next();
            if next_raw == Some(entry.raw_record_position) {
                expected_raw_position = entry.raw_record_position;
                expected_within_position = ImportedRecordEntryPosition::first();
            } else {
                return Err(
                    ImportedConversationReconstitutionFailure::EntryRawRecordPositionMismatch {
                        entry: entry.identity,
                        expected: next_raw.unwrap_or(expected_raw_position),
                        actual: entry.raw_record_position,
                    },
                );
            }
        }
        if entry.record_entry_position != expected_within_position {
            return Err(
                ImportedConversationReconstitutionFailure::EntryWithinRecordPositionMismatch {
                    entry: entry.identity,
                    expected: expected_within_position,
                    actual: entry.record_entry_position,
                },
            );
        }
        validate_speaker(input, entry)?;
        validate_entry_depth(entry)?;
        if entry.raw_record_position != projected_raw_position {
            if projected_entry_index != expected_entries.len() {
                return Err(
                    ImportedConversationReconstitutionFailure::RawRecordEntryProjectionMismatch {
                        position: projected_raw_position,
                    },
                );
            }
            let raw_index = usize::try_from(entry.raw_record_position.as_u64() - 1)
                .map_err(|_| ImportedConversationReconstitutionFailure::PositionExhausted)?;
            let record = input.raw_records.get(raw_index).ok_or(
                ImportedConversationReconstitutionFailure::EntryRawRecordNotFound {
                    entry: entry.identity,
                    position: entry.raw_record_position,
                },
            )?;
            expected_entries =
                projected_entries(input.format, record.normalized()).map_err(|()| {
                    ImportedConversationReconstitutionFailure::RawRecordProjectionInvalid {
                        position: record.position,
                    }
                })?;
            projected_raw_position = record.position;
            projected_entry_index = 0;
        }
        let expected_entry = expected_entries.get(projected_entry_index).ok_or(
            ImportedConversationReconstitutionFailure::RawRecordEntryProjectionMismatch {
                position: projected_raw_position,
            },
        )?;
        if expected_entry.source_speaker != entry.source_speaker
            || expected_entry.content != entry.content
            || expected_entry.source != entry.source
        {
            return Err(
                ImportedConversationReconstitutionFailure::EntryProjectionMismatch {
                    entry: entry.identity,
                },
            );
        }
        projected_entry_index = projected_entry_index
            .checked_add(1)
            .ok_or(ImportedConversationReconstitutionFailure::PositionExhausted)?;

        if let Some(next_entry) = input.entries.get(index + 1) {
            expected_position = expected_position
                .checked_next()
                .ok_or(ImportedConversationReconstitutionFailure::PositionExhausted)?;
            if next_entry.raw_record_position == expected_raw_position {
                expected_within_position = expected_within_position
                    .checked_next()
                    .ok_or(ImportedConversationReconstitutionFailure::PositionExhausted)?;
            }
        }
    }

    if expected_raw_position != last_raw_position {
        return Err(
            ImportedConversationReconstitutionFailure::RawRecordWithoutEntry {
                position: expected_raw_position
                    .checked_next()
                    .ok_or(ImportedConversationReconstitutionFailure::PositionExhausted)?,
            },
        );
    }
    if projected_entry_index != expected_entries.len() {
        return Err(
            ImportedConversationReconstitutionFailure::RawRecordEntryProjectionMismatch {
                position: projected_raw_position,
            },
        );
    }
    Ok(())
}

fn validate_speaker(
    input: &ImportedConversationReconstitutionInput,
    entry: &ImportedTranscriptEntryInput,
) -> Result<(), ImportedConversationReconstitutionFailure> {
    let record = input
        .raw_records
        .get(
            usize::try_from(entry.raw_record_position.as_u64() - 1)
                .map_err(|_| ImportedConversationReconstitutionFailure::PositionExhausted)?,
        )
        .ok_or(
            ImportedConversationReconstitutionFailure::EntryRawRecordNotFound {
                entry: entry.identity,
                position: entry.raw_record_position,
            },
        )?;
    let record_speaker =
        normalized_record_speaker(input.format, record.normalized()).map_err(|()| {
            ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch {
                entry: entry.identity,
            }
        })?;

    if let ImportedTranscriptContent::SourceEvent { source_type } = &entry.content {
        if entry.source_speaker != ImportedSourceAttestation::NotAttested {
            return Err(
                ImportedConversationReconstitutionFailure::SourceEventSpeakerMismatch {
                    entry: entry.identity,
                },
            );
        }
        let record_type = normalized_record_type(record.normalized()).map_err(|()| {
            ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch {
                entry: entry.identity,
            }
        })?;
        if record_speaker.is_some() || *source_type != record_type {
            return Err(
                ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch {
                    entry: entry.identity,
                },
            );
        }
        return Ok(());
    }

    match (record_speaker, &entry.source_speaker) {
        (Some(record_speaker), ImportedSourceAttestation::Attested(entry_speaker))
            if record_speaker == *entry_speaker =>
        {
            if let ImportedSourceAttestation::Attested(message_role) = entry.source.message_role
                && message_role != *entry_speaker
            {
                return Err(
                    ImportedConversationReconstitutionFailure::MessageRoleMismatch {
                        entry: entry.identity,
                    },
                );
            }
        }
        (None, ImportedSourceAttestation::NotAttested) => {}
        (Some(_), ImportedSourceAttestation::NotAttested)
        | (Some(_), ImportedSourceAttestation::AttestedAbsent)
        | (None, ImportedSourceAttestation::Attested(_))
        | (None, ImportedSourceAttestation::AttestedAbsent) => {
            return Err(
                ImportedConversationReconstitutionFailure::MessageSpeakerUnavailable {
                    entry: entry.identity,
                },
            );
        }
        (Some(_), ImportedSourceAttestation::Attested(_)) => {
            return Err(
                ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch {
                    entry: entry.identity,
                },
            );
        }
    }
    Ok(())
}

fn normalized_record_type(
    normalized: &ImportedStructuredValue,
) -> Result<ImportedSourceAttestation<ImportedText>, ()> {
    let ImportedStructuredValue::Object(members) = normalized else {
        return Err(());
    };
    imported_text_attestation(members, "type").map_err(|_| ())
}

fn normalized_record_speaker(
    format: ImportedConversationFormat,
    normalized: &ImportedStructuredValue,
) -> Result<Option<ImportedSpeaker>, ()> {
    if matches!(
        format,
        ImportedConversationFormat::CodexRolloutJsonlV1
            | ImportedConversationFormat::CodexRolloutJsonlV2
    ) {
        return normalized_codex_record_speaker(normalized);
    }
    match normalized_record_type(normalized)? {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "user" => {
            Ok(Some(ImportedSpeaker::User))
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "assistant" => {
            Ok(Some(ImportedSpeaker::Assistant))
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => Ok(None),
    }
}

fn normalized_codex_record_speaker(
    normalized: &ImportedStructuredValue,
) -> Result<Option<ImportedSpeaker>, ()> {
    let ImportedStructuredValue::Object(record) = normalized else {
        return Err(());
    };
    if !matches!(
        projected_text_attestation(record, "type")?,
        ImportedSourceAttestation::Attested(value) if value.as_str() == "response_item"
    ) {
        return Ok(None);
    }
    let payload = match unique_structured_field(record, "payload")? {
        Some(ImportedStructuredValue::Object(payload)) => payload,
        None | Some(_) => return Err(()),
    };
    if !matches!(
        projected_text_attestation(payload, "type")?,
        ImportedSourceAttestation::Attested(value) if value.as_str() == "message"
    ) {
        return Ok(None);
    }
    match projected_text_attestation(payload, "role")? {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "user" => {
            Ok(Some(ImportedSpeaker::User))
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "assistant" => {
            Ok(Some(ImportedSpeaker::Assistant))
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => Ok(None),
    }
}

fn structured_value_within_depth(value: &ImportedStructuredValue) -> bool {
    let mut pending = vec![(value, 0_usize)];
    while let Some((value, depth)) = pending.pop() {
        match value {
            ImportedStructuredValue::Array(values) => {
                let Some(depth) = depth.checked_add(1) else {
                    return false;
                };
                if depth > MAX_STRUCTURED_CONTAINER_DEPTH {
                    return false;
                }
                pending.extend(values.iter().map(|value| (value, depth)));
            }
            ImportedStructuredValue::Object(members) => {
                let Some(depth) = depth.checked_add(1) else {
                    return false;
                };
                if depth > MAX_STRUCTURED_CONTAINER_DEPTH {
                    return false;
                }
                pending.extend(members.iter().map(|member| (member.value(), depth)));
            }
            ImportedStructuredValue::Null
            | ImportedStructuredValue::Boolean(_)
            | ImportedStructuredValue::Number(_)
            | ImportedStructuredValue::String(_) => {}
        }
    }
    true
}

fn validate_entry_depth(
    entry: &ImportedTranscriptEntryInput,
) -> Result<(), ImportedConversationReconstitutionFailure> {
    let within_bound = match &entry.content {
        ImportedTranscriptContent::ToolCall { input, caller, .. } => {
            structured_attestation_within_depth(input)
                && structured_attestation_within_depth(caller)
        }
        ImportedTranscriptContent::SourceEvent { .. }
        | ImportedTranscriptContent::SourceMessageBlock { .. }
        | ImportedTranscriptContent::Text(_)
        | ImportedTranscriptContent::ToolResult { .. }
        | ImportedTranscriptContent::Thinking { .. }
        | ImportedTranscriptContent::RedactedThinking { .. }
        | ImportedTranscriptContent::Document { .. }
        | ImportedTranscriptContent::MessageContentAbsent(_) => true,
    };
    if within_bound {
        Ok(())
    } else {
        Err(
            ImportedConversationReconstitutionFailure::EntryStructuredValueDepthExceeded {
                entry: entry.identity,
            },
        )
    }
}

fn structured_attestation_within_depth(
    value: &ImportedSourceAttestation<ImportedStructuredValue>,
) -> bool {
    match value {
        ImportedSourceAttestation::Attested(value) => structured_value_within_depth(value),
        ImportedSourceAttestation::AttestedAbsent | ImportedSourceAttestation::NotAttested => true,
    }
}
