use serde::Deserialize;
use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ValidationEvidence, VerifiedBlobSource,
};

use crate::{JSON_MEDIA_TYPE, MAX_TEXT_FAMILY_BYTES, options_are_empty, source};

pub(crate) const MAX_STRUCTURED_DEPTH: u32 = 64;

pub(crate) async fn probe(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorProbeOutput, ProcessorFailure> {
    let prefix = source::read_probe_prefix(source, cancellation).await?;
    let candidate = has_json_structure(&prefix);
    if candidate {
        Ok(ProcessorProbeOutput::Candidate {
            media_type: String::from(JSON_MEDIA_TYPE),
            strength: ProbeStrength::StructuralCandidate,
        })
    } else {
        Ok(ProcessorProbeOutput::NoMatch)
    }
}

fn has_json_structure(prefix: &[u8]) -> bool {
    let prefix = trim_ascii_start(prefix);
    if let Some(value_end) = parse_value_prefix(prefix) {
        return prefix[value_end..].iter().all(u8::is_ascii_whitespace);
    }
    let Some((&opening, rest)) = prefix.split_first() else {
        return false;
    };
    let rest = trim_ascii_start(rest);
    match (opening, rest.first()) {
        (b'{', Some(_)) => rest.iter().enumerate().any(|(index, byte)| {
            *byte == b':' && serde_json::from_slice::<String>(&rest[..index]).is_ok()
        }),
        (b'[', Some(_)) => has_array_structure(rest),
        _ => false,
    }
}

fn has_array_structure(rest: &[u8]) -> bool {
    let Some(first_end) = parse_value_prefix(rest) else {
        return false;
    };
    let after_first = trim_ascii_start(&rest[first_end..]);
    match after_first.split_first() {
        Some((b']', trailing)) => trailing.iter().all(u8::is_ascii_whitespace),
        Some((b',', after_comma)) => {
            let after_comma = trim_ascii_start(after_comma);
            let Some(second_end) = parse_value_prefix(after_comma) else {
                return false;
            };
            let after_second = trim_ascii_start(&after_comma[second_end..]);
            match after_second.split_first() {
                Some((b',', _)) => true,
                Some((b']', trailing)) => trailing.iter().all(u8::is_ascii_whitespace),
                _ => false,
            }
        }
        _ => false,
    }
}

fn parse_value_prefix(bytes: &[u8]) -> Option<usize> {
    let mut values = serde_json::Deserializer::from_slice(bytes).into_iter::<serde_json::Value>();
    values.next()?.ok()?;
    Some(values.byte_offset())
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
