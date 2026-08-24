use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput,
    ValidationEvidence, VerifiedBlobSource,
};

use crate::{MAX_TEXT_FAMILY_BYTES, TEXT_MEDIA_TYPE, TEXT_VIEW_NAME, read_input_is_empty, source};

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
    let Some(bytes) = source::read_complete(source, cancellation).await? else {
        return Ok(validation_failure(&request, "source_too_large"));
    };
    match source::checked_utf8(bytes) {
        Ok(text) => Ok(ProcessorValidationOutput::Validated {
            media_type: String::from(TEXT_MEDIA_TYPE),
            evidence: request.evidence,
            metadata_json: serde_json::json!({"bytes": text.len()}).to_string(),
        }),
        Err(reason) => Ok(validation_failure(&request, reason)),
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
    if request.view.as_str() != TEXT_VIEW_NAME || !read_input_is_empty(&request.input) {
        return Ok(ProcessorReadOutput::InvalidViewArguments);
    }
    let Some(bytes) = source::read_complete(source, cancellation).await? else {
        return Ok(ProcessorReadOutput::SourceTooLarge {
            maximum_bytes: MAX_TEXT_FAMILY_BYTES,
        });
    };
    let text = source::checked_utf8(bytes).map_err(|_| ProcessorFailure::Failed)?;
    Ok(ProcessorReadOutput::Text {
        body: text,
        truncated: false,
        cursor: None,
    })
}
