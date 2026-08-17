use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, VerifiedBlobSource,
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
    let mut bytes = prefix
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace());
    match (bytes.next(), bytes.next()) {
        (Some(b'{'), Some(next)) => matches!(next, b'}' | b'"'),
        (Some(b'['), Some(next)) => matches!(
            next,
            b']' | b'{' | b'[' | b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n'
        ),
        _ => false,
    }
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
        return Ok(malformed("source_too_large"));
    };
    let text = match source::checked_utf8(bytes) {
        Ok(text) => text,
        Err(reason) => return Ok(malformed(reason)),
    };
    if serde_json::from_str::<serde_json::Value>(&text).is_err() {
        return Ok(malformed("malformed_json"));
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
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| ProcessorFailure::Failed)?;
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
