use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput,
    ValidationEvidence, VerifiedBlobSource,
};

use crate::{MAX_TEXT_FAMILY_BYTES, TEXT_MEDIA_TYPE, TEXT_VIEW_NAME, options_are_empty, source};
use signalbox_file_media_runtime::FileReadInput;
use std::num::NonZeroU64;

// A section reserves three bytes to complete a UTF-8 scalar at its boundary.
const SECTION_BYTES: u64 = MAX_TEXT_FAMILY_BYTES - 3;

pub(crate) async fn probe(
    _source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorProbeOutput, ProcessorFailure> {
    if cancellation.is_cancelled() {
        Err(ProcessorFailure::Cancelled)
    } else {
        Ok(ProcessorProbeOutput::NoMatch)
    }
}

pub(crate) async fn inspect(
    request: FileMediaProviderValidationRequest,
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
    if request.media_type.as_str() != TEXT_MEDIA_TYPE {
        return Err(ProcessorFailure::Protocol);
    }
    let bytes =
        source::read_validation_prefix(source, cancellation, request.maximum_source_bytes).await?;
    let text = if (bytes.len() as u64) < source.byte_length().get() {
        source::probe_utf8(&bytes)
    } else {
        std::str::from_utf8(&bytes).ok()
    };
    match text {
        Some(text) if !text.contains('\0') => Ok(ProcessorValidationOutput::Validated {
            media_type: String::from(TEXT_MEDIA_TYPE),
            evidence: request.evidence,
            metadata_json: serde_json::json!({"validated_prefix_bytes": text.len(), "source_bytes": source.byte_length().get()}).to_string(),
        }),
        Some(_) => Ok(validation_failure(&request, "nul_byte")),
        None => Ok(validation_failure(&request, "invalid_utf8")),
    }
}

fn validation_failure(
    request: &FileMediaProviderValidationRequest,
    reason: &str,
) -> ProcessorValidationOutput {
    match request.evidence {
        ValidationEvidence::StreamingTextValidation => ProcessorValidationOutput::NoMatch,
        ValidationEvidence::StrongSignature
        | ValidationEvidence::StructuralValidation
        | ValidationEvidence::DeclaredCandidateStructurallyValidated => {
            ProcessorValidationOutput::Malformed {
                media_type: String::from(TEXT_MEDIA_TYPE),
                reason_code: String::from(reason),
            }
        }
    }
}

pub(crate) async fn read(
    request: FileMediaProviderReadRequest,
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorReadOutput, ProcessorFailure> {
    if request.view.as_str() != TEXT_VIEW_NAME {
        return Ok(ProcessorReadOutput::InvalidViewArguments);
    }
    let section = match &request.input {
        FileReadInput::Initial { options } if options_are_empty(options) => 0,
        FileReadInput::Continuation { cursor } => {
            let Some(section) = cursor
                .as_str()
                .strip_prefix("section_")
                .and_then(|value| value.parse::<u64>().ok())
            else {
                return Ok(ProcessorReadOutput::InvalidViewArguments);
            };
            section
        }
        _ => return Ok(ProcessorReadOutput::InvalidViewArguments),
    };
    let Some(offset) = section
        .checked_mul(SECTION_BYTES)
        .filter(|offset| *offset < source.byte_length().get())
    else {
        return Ok(ProcessorReadOutput::InvalidViewArguments);
    };
    if cancellation.is_cancelled() {
        return Err(ProcessorFailure::Cancelled);
    }
    let length = NonZeroU64::new((source.byte_length().get() - offset).min(MAX_TEXT_FAMILY_BYTES))
        .ok_or(ProcessorFailure::Failed)?;
    let bytes = source
        .read_range(offset, length)
        .await
        .map_err(|_| ProcessorFailure::Failed)?;
    let continuation_byte = |byte: &u8| byte & 0xc0 == 0x80;
    let start = if section == 0 {
        0
    } else {
        bytes
            .iter()
            .take(3)
            .take_while(|byte| continuation_byte(byte))
            .count()
    };
    let mut end = bytes.len().min(SECTION_BYTES as usize);
    while end < bytes.len() && continuation_byte(&bytes[end]) {
        end += 1;
    }
    let text = std::str::from_utf8(&bytes[start..end]).map_err(|_| ProcessorFailure::Failed)?;
    if text.contains('\0') {
        return Err(ProcessorFailure::Failed);
    }
    let truncated = offset + (end as u64) < source.byte_length().get();
    Ok(ProcessorReadOutput::Text {
        body: text.to_owned(),
        truncated,
        cursor: truncated.then(|| format!("section_{}", section + 1)),
    })
}
