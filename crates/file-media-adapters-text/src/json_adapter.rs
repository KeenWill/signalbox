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
    let Some((&opening, rest)) = prefix.split_first() else {
        return false;
    };
    let rest = trim_ascii_start(rest);
    match (opening, rest.first()) {
        (b'{', Some(b'}')) | (b'[', Some(b']')) => true,
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
        Some((b']', _)) => true,
        Some((b',', after_comma)) => {
            let after_comma = trim_ascii_start(after_comma);
            let Some(second_end) = parse_value_prefix(after_comma) else {
                return false;
            };
            matches!(
                trim_ascii_start(&after_comma[second_end..]).first(),
                Some(b',') | Some(b']')
            )
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
    if parse_json(&text).is_err() {
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
    let value = parse_json(&text).map_err(|_| ProcessorFailure::Failed)?;
    if json_depth(&value, 1) > MAX_STRUCTURED_DEPTH {
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

fn parse_json(text: &str) -> Result<serde_json::Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    let value =
        serde_json::Value::deserialize(serde_stacker::Deserializer::new(&mut deserializer))?;
    deserializer.end()?;
    Ok(value)
}

fn json_depth(value: &serde_json::Value, depth: u32) -> u32 {
    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| json_depth(value, depth.saturating_add(1)))
            .max()
            .unwrap_or(depth),
        serde_json::Value::Object(values) => values
            .values()
            .map(|value| json_depth(value, depth.saturating_add(1)))
            .max()
            .unwrap_or(depth),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => depth,
    }
}

fn malformed(reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(JSON_MEDIA_TYPE),
        reason_code: String::from(reason),
    }
}

fn validation_failure(evidence: ValidationEvidence, reason: &str) -> ProcessorValidationOutput {
    if evidence == ValidationEvidence::DeclaredCandidateStructurallyValidated {
        ProcessorValidationOutput::NoMatch
    } else {
        malformed(reason)
    }
}
