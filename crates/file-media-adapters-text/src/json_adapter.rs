use serde::Deserialize;
use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    MAX_OBSERVED_CONTAINER_ENTRIES, ProbeStrength, ProcessorFailure, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ValidationEvidence, VerifiedBlobSource,
};

use crate::{JSON_MEDIA_TYPE, MAX_TEXT_FAMILY_BYTES, options_are_empty, source};

pub(crate) const MAX_STRUCTURED_DEPTH: u32 = 64;

pub(crate) async fn probe(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorProbeOutput, ProcessorFailure> {
    let prefix = source::read_probe_prefix(source, cancellation).await?;
    let complete = source.byte_length().get() <= prefix.len() as u64;
    let candidate = has_json_structure(&prefix, complete);
    if candidate {
        Ok(ProcessorProbeOutput::Candidate {
            media_type: String::from(JSON_MEDIA_TYPE),
            strength: ProbeStrength::StructuralCandidate,
        })
    } else {
        Ok(ProcessorProbeOutput::NoMatch)
    }
}

fn has_json_structure(prefix: &[u8], complete: bool) -> bool {
    let prefix = trim_ascii_start(prefix);
    let Ok(text) = std::str::from_utf8(prefix) else {
        return false;
    };
    if complete {
        return validate_json(text).is_ok();
    }
    if !matches!(prefix.first(), Some(b'{' | b'[')) {
        return false;
    }
    incomplete_json_prefix(text)
}

fn incomplete_json_prefix(text: &str) -> bool {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    serde::de::IgnoredAny::deserialize(serde_stacker::Deserializer::new(&mut deserializer))
        .is_err_and(|error| error.is_eof())
}

fn trim_ascii_start(bytes: &[u8]) -> &[u8] {
    let first = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[first..]
}

pub(crate) async fn inspect(
    request: FileMediaProviderValidationRequest,
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
    if request.media_type.as_str() != JSON_MEDIA_TYPE {
        return Err(ProcessorFailure::Protocol);
    }
    let Some(bytes) = source::read_complete(source, cancellation).await? else {
        return Ok(validation_failure(request.evidence, "source_too_large"));
    };
    let text = match source::checked_utf8(bytes) {
        Ok(text) => text,
        Err(reason) => return Ok(validation_failure(request.evidence, reason)),
    };
    if validate_json(&text).is_err() {
        return Ok(validation_failure(request.evidence, "malformed_json"));
    }
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(JSON_MEDIA_TYPE),
        evidence: request.evidence,
        metadata_json: serde_json::json!({"bytes": text.len()}).to_string(),
    })
}

pub(crate) async fn read(
    request: FileMediaProviderReadRequest,
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorReadOutput, ProcessorFailure> {
    if request.view.as_str() != "structured" || !options_are_empty(&request.options) {
        return Ok(ProcessorReadOutput::InvalidViewArguments);
    }
    let Some(bytes) = source::read_complete(source, cancellation).await? else {
        return Ok(ProcessorReadOutput::SourceTooLarge {
            maximum_bytes: MAX_TEXT_FAMILY_BYTES,
        });
    };
    let text = source::checked_utf8(bytes).map_err(|_| ProcessorFailure::Failed)?;
    if json_depth_exceeds(text.as_bytes(), MAX_STRUCTURED_DEPTH) {
        return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
            limit_kind: String::from("depth_limit_exceeded"),
        });
    }
    let value = parse_json(&text).map_err(|_| ProcessorFailure::Failed)?;
    if json_value_depth_exceeds(&value, MAX_STRUCTURED_DEPTH) {
        return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
            limit_kind: String::from("depth_limit_exceeded"),
        });
    }
    if json_container_entries_exceed(&value, MAX_OBSERVED_CONTAINER_ENTRIES) {
        return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
            limit_kind: String::from("container_entry_limit_exceeded"),
        });
    }
    let body_json = serde_json::to_string(&value).map_err(|_| ProcessorFailure::Failed)?;
    if body_json.len() > MAX_TEXT_FAMILY_BYTES as usize {
        return Ok(ProcessorReadOutput::OutputUnitTooLarge);
    }
    Ok(ProcessorReadOutput::Structured {
        body_json,
        truncated: false,
        cursor: None,
    })
}

fn validate_json(text: &str) -> Result<(), serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    serde::de::IgnoredAny::deserialize(serde_stacker::Deserializer::new(&mut deserializer))?;
    deserializer.end()
}

fn parse_json(text: &str) -> Result<serde_json::Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    let value =
        serde_json::Value::deserialize(serde_stacker::Deserializer::new(&mut deserializer))?;
    deserializer.end()?;
    Ok(value)
}

/// Detects excessive nesting before building a recursively dropped JSON tree.
fn json_depth_exceeds(bytes: &[u8], maximum_depth: u32) -> bool {
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut escaped = false;
    for byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'\"' {
                in_string = false;
            }
        } else if *byte == b'\"' {
            in_string = true;
        } else if matches!(*byte, b'{' | b'[') {
            depth = depth.saturating_add(1);
            if depth > maximum_depth {
                return true;
            }
        } else if matches!(*byte, b'}' | b']') {
            depth = depth.saturating_sub(1);
        }
    }
    false
}

/// Measures admitted trees iteratively so depth enforcement cannot overflow the stack.
fn json_value_depth_exceeds(value: &serde_json::Value, maximum_depth: u32) -> bool {
    let mut pending = vec![(value, 1_u32)];
    while let Some((value, depth)) = pending.pop() {
        if depth > maximum_depth {
            return true;
        }
        let child_depth = depth.saturating_add(1);
        match value {
            serde_json::Value::Array(values) => {
                pending.extend(values.iter().map(|value| (value, child_depth)));
            }
            serde_json::Value::Object(values) => {
                pending.extend(values.values().map(|value| (value, child_depth)));
            }
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => {}
        }
    }
    false
}

/// Checks concentrated container fan-out iteratively before output crosses the worker.
fn json_container_entries_exceed(value: &serde_json::Value, maximum_entries: u64) -> bool {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::Array(values) => {
                if values.len() as u64 > maximum_entries {
                    return true;
                }
                pending.extend(values);
            }
            serde_json::Value::Object(values) => {
                if values.len() as u64 > maximum_entries {
                    return true;
                }
                pending.extend(values.values());
            }
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => {}
        }
    }
    false
}

fn malformed(reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(JSON_MEDIA_TYPE),
        reason_code: String::from(reason),
    }
}

fn validation_failure(evidence: ValidationEvidence, reason: &str) -> ProcessorValidationOutput {
    match evidence {
        ValidationEvidence::DeclaredCandidateStructurallyValidated => {
            ProcessorValidationOutput::NoMatch
        }
        ValidationEvidence::StrongSignature
        | ValidationEvidence::StructuralValidation
        | ValidationEvidence::StreamingTextValidation => malformed(reason),
    }
}
