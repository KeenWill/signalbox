use std::{error::Error, num::NonZeroU64, sync::Arc};

use signalbox_file_media_adapters_text::{TextFamilyProvider, text_family_declaration};
use signalbox_file_media_runtime::{
    AttachmentKind, CancellationSignal, DeclaredMediaType, FileDigest, FileInspection,
    FileMediaCeilings, FileMediaFailure, FileMediaProcessor, FileMediaProcessorFuture,
    FileMediaProvider, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    FileMediaRegistry, FileReadInput, FileReadRequest, FileReadResult, FileUse, InspectionRequest,
    NeverCancelled, ProcessorBoundaryFailure, ProcessorFailure, ProcessorIsolation,
    ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput, ReadViewName,
    ReaderIdentity, SourceReadError, SourceReadFuture, VerifiedBlobSource,
};

pub(crate) struct MemorySource {
    bytes: Arc<[u8]>,
}

impl MemorySource {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::from(bytes),
        }
    }

    pub(crate) fn file_use(&self, media_type: &str) -> Result<FileUse, Box<dyn Error>> {
        Ok(FileUse::new(
            self.digest(),
            self.byte_length(),
            AttachmentKind::Document,
            DeclaredMediaType::try_new(media_type)?,
            None,
        ))
    }
}

impl VerifiedBlobSource for MemorySource {
    fn digest(&self) -> FileDigest {
        FileDigest::from_bytes([7; 32])
    }

    fn byte_length(&self) -> NonZeroU64 {
        NonZeroU64::new(self.bytes.len() as u64).unwrap_or(NonZeroU64::MIN)
    }

    fn read_range(&self, offset: u64, length: NonZeroU64) -> SourceReadFuture<'_> {
        Box::pin(async move {
            let start = usize::try_from(offset).map_err(|_| SourceReadError::RangeOutOfBounds)?;
            let requested =
                usize::try_from(length.get()).map_err(|_| SourceReadError::RangeOutOfBounds)?;
            let end = start
                .checked_add(requested)
                .ok_or(SourceReadError::RangeOutOfBounds)?;
            self.bytes
                .get(start..end)
                .map(<[u8]>::to_vec)
                .ok_or(SourceReadError::RangeOutOfBounds)
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ReadBehavior {
    Provider,
    InjectedStructured(String),
}

pub(crate) struct DirectProcessor {
    provider: TextFamilyProvider,
    read_behavior: ReadBehavior,
}

impl DirectProcessor {
    pub(crate) fn provider() -> Self {
        Self {
            provider: TextFamilyProvider,
            read_behavior: ReadBehavior::Provider,
        }
    }

    pub(crate) fn injecting(body_json: String) -> Self {
        Self {
            provider: TextFamilyProvider,
            read_behavior: ReadBehavior::InjectedStructured(body_json),
        }
    }
}

impl FileMediaProcessor for DirectProcessor {
    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        Box::pin(async move {
            self.provider
                .probe(reader, source, cancellation)
                .await
                .map_err(|_| ProcessorBoundaryFailure::Processor(ProcessorFailure::Failed))
        })
    }

    fn validate<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        Box::pin(async move {
            self.provider
                .inspect(reader, request, source, cancellation)
                .await
                .map_err(|_| ProcessorBoundaryFailure::Processor(ProcessorFailure::Failed))
        })
    }

    fn read<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderReadRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        match &self.read_behavior {
            ReadBehavior::Provider => Box::pin(async move {
                self.provider
                    .read(reader, request, source, cancellation)
                    .await
                    .map_err(|_| ProcessorBoundaryFailure::Processor(ProcessorFailure::Failed))
            }),
            ReadBehavior::InjectedStructured(body_json) => {
                let body_json = body_json.clone();
                Box::pin(async move {
                    Ok(ProcessorReadOutput::Structured {
                        body_json,
                        truncated: false,
                        cursor: None,
                    })
                })
            }
        }
    }
}

pub(crate) fn registry() -> Result<FileMediaRegistry, Box<dyn Error>> {
    Ok(FileMediaRegistry::try_new(
        vec![text_family_declaration().map_err(|error| error.to_string())?],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )?)
}

pub(crate) async fn inspect(
    source: &MemorySource,
    media_type: &str,
) -> Result<FileInspection, Box<dyn Error>> {
    Ok(registry()?
        .inspect(
            &DirectProcessor::provider(),
            InspectionRequest {
                source: source.file_use(media_type)?,
                visible_part: None,
            },
            source,
            &NeverCancelled,
        )
        .await?)
}

pub(crate) struct ReadInput<'a> {
    pub(crate) media_type: &'a str,
    pub(crate) view: &'a str,
}

pub(crate) async fn read(
    source: &MemorySource,
    input: ReadInput<'_>,
    processor: &DirectProcessor,
) -> Result<FileReadResult, FileMediaFailure> {
    let source_use = source
        .file_use(input.media_type)
        .map_err(|_| FileMediaFailure::ProcessorFailed)?;
    let view = ReadViewName::try_new(input.view).map_err(|_| FileMediaFailure::ProcessorFailed)?;
    registry()
        .map_err(|_| FileMediaFailure::ProcessorFailed)?
        .read(
            processor,
            FileReadRequest {
                inspection: InspectionRequest {
                    source: source_use,
                    visible_part: None,
                },
                view,
                input: FileReadInput::Initial {
                    options: serde_json::json!({}),
                },
            },
            source,
            &NeverCancelled,
        )
        .await
}

#[track_caller]
pub(crate) fn assert_validated_media(inspection: FileInspection, expected: &str) {
    assert!(matches!(inspection, FileInspection::Validated(_)));
    if let FileInspection::Validated(validated) = inspection {
        assert_eq!(validated.detected_media_type().as_str(), expected);
    }
}

#[track_caller]
pub(crate) fn assert_malformed_reason(inspection: FileInspection, expected: &str) {
    assert!(matches!(inspection, FileInspection::Malformed { .. }));
    if let FileInspection::Malformed { reason_code, .. } = inspection {
        assert_eq!(reason_code.as_str(), expected);
    }
}

#[track_caller]
pub(crate) fn assert_unknown(inspection: FileInspection) {
    assert!(matches!(inspection, FileInspection::Unknown { .. }));
}

pub(crate) struct DeclaredMismatchExpectation<'a> {
    pub(crate) declared: &'a str,
    pub(crate) detected: &'a str,
}

#[track_caller]
pub(crate) fn assert_declared_mismatch(
    inspection: FileInspection,
    expected: DeclaredMismatchExpectation<'_>,
) {
    assert!(matches!(
        inspection,
        FileInspection::DeclaredMismatch { .. }
    ));
    if let FileInspection::DeclaredMismatch {
        declared, detected, ..
    } = inspection
    {
        assert_eq!(declared.as_str(), expected.declared);
        assert_eq!(detected.as_str(), expected.detected);
    }
}

#[track_caller]
pub(crate) fn assert_text(result: FileReadResult, expected: &str) {
    assert!(matches!(result, FileReadResult::Text { .. }));
    if let FileReadResult::Text { body, .. } = result {
        assert_eq!(body, expected);
    }
}

#[track_caller]
pub(crate) fn assert_structured(result: FileReadResult, expected: &serde_json::Value) {
    assert!(matches!(result, FileReadResult::Structured { .. }));
    if let FileReadResult::Structured { body, .. } = result {
        assert_eq!(&body, expected);
    }
}

#[track_caller]
pub(crate) fn assert_structured_json(result: FileReadResult, expected: &str) {
    assert!(matches!(result, FileReadResult::Structured { .. }));
    if let FileReadResult::Structured { body, .. } = result {
        assert_eq!(body.to_string(), expected);
    }
}

#[track_caller]
pub(crate) fn assert_processor_failed(result: Result<FileReadResult, FileMediaFailure>) {
    assert_eq!(result, Err(FileMediaFailure::ProcessorFailed));
}
