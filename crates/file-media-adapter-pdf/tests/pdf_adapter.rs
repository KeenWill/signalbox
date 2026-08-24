mod fixtures;

use std::error::Error;

use fixtures::{MemorySource, PdfFixture};
use signalbox_file_media_adapter_pdf::{PdfProvider, declaration};
use signalbox_file_media_runtime::{
    CancellationSignal, FileInspection, FileInspectionStatus, FileMediaCeilings, FileMediaFailure,
    FileMediaProcessor, FileMediaProcessorFuture, FileMediaProvider, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileMediaRegistry, FileReadInput, FileReadRequest,
    FileReadResult, InspectionRequest, NeverCancelled, ProcessorBoundaryFailure, ProcessorFailure,
    ProcessorIsolation, ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput,
    ReadContinuation, ReadViewName, ReaderIdentity, ReasonCode, VerifiedBlobSource,
};

struct DirectProcessor {
    provider: PdfProvider,
}

impl DirectProcessor {
    const fn new() -> Self {
        Self {
            provider: PdfProvider::new(),
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
        Box::pin(async move {
            self.provider
                .read(reader, request, source, cancellation)
                .await
                .map_err(|_| ProcessorBoundaryFailure::Processor(ProcessorFailure::Failed))
        })
    }
}

struct AdversarialOutputProcessor {
    direct: DirectProcessor,
}

impl AdversarialOutputProcessor {
    const fn new() -> Self {
        Self {
            direct: DirectProcessor::new(),
        }
    }
}

impl FileMediaProcessor for AdversarialOutputProcessor {
    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        self.direct.probe(reader, source, cancellation)
    }

    fn validate<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        self.direct.validate(reader, request, source, cancellation)
    }

    fn read<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _request: FileMediaProviderReadRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        Box::pin(async {
            Ok(ProcessorReadOutput::Text {
                body: String::from("decoder\0injection"),
                truncated: false,
                cursor: None,
            })
        })
    }
}

#[test]
fn declaration_registers_under_the_available_isolation_contract() -> Result<(), Box<dyn Error>> {
    let registry = registry()?;

    assert_eq!(registry.providers(), &[declaration()?]);
    Ok(())
}

#[tokio::test]
async fn generated_pdf_validates_and_exposes_declared_views() -> Result<(), Box<dyn Error>> {
    let fixture = PdfFixture::ordinary()?;
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    assert_eq!(validated_view_count(&inspection)?, 2);
    Ok(())
}

#[tokio::test]
async fn generated_pdf_text_is_extracted_without_ocr() -> Result<(), Box<dyn Error>> {
    let fixture = PdfFixture::ordinary()?;
    let expected_text = fixture.expected_text();
    let source = fixture.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await?;
    let body = complete_text(result)?;

    assert!(body.contains(expected_text));
    Ok(())
}

#[tokio::test]
async fn generated_pdf_metadata_reports_fixture_page_count() -> Result<(), Box<dyn Error>> {
    let fixture = PdfFixture::ordinary()?;
    let expected_pages = fixture.expected_page_count();
    let source = fixture.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;
    let body = complete_structure(result)?;

    assert_eq!(body["pages"], expected_pages);
    Ok(())
}

#[tokio::test]
async fn truncated_pdf_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    let source = PdfFixture::truncated()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    assert_eq!(malformed_reason(&inspection)?, "malformed_pdf");
    Ok(())
}

#[tokio::test]
async fn false_pdf_signature_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"%PDF-bad".to_vec())?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    Ok(())
}

#[tokio::test]
async fn unknown_bytes_remain_a_typed_unknown_inspection() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"not a PDF".to_vec())?;
    let inspection =
        inspect_as(&DirectProcessor::new(), &source, "application/octet-stream").await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn locked_pdf_is_terminal_without_a_password_channel() -> Result<(), Box<dyn Error>> {
    let source = PdfFixture::locked()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::EncryptedOrLocked);
    Ok(())
}

#[tokio::test]
async fn encrypt_token_in_page_text_does_not_claim_a_locked_trailer() -> Result<(), Box<dyn Error>>
{
    let source = PdfFixture::with_text("literal /Encrypt content")?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn encrypt_token_in_trailer_comment_does_not_claim_a_locked_file()
-> Result<(), Box<dyn Error>> {
    let source = PdfFixture::trailer_encrypt_comment()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn token_shaped_large_garbage_is_not_structurally_validated() -> Result<(), Box<dyn Error>> {
    let source = PdfFixture::malformed_large()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    Ok(())
}

#[tokio::test]
async fn bounded_validation_stays_within_its_declared_source_budget() -> Result<(), Box<dyn Error>>
{
    let fixture = PdfFixture::over_source_limit()?;
    let expected_limit = fixture.expected_validation_source_limit();
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    assert!(source.requested_bytes() <= expected_limit + 8);
    Ok(())
}

#[tokio::test]
async fn malformed_large_trailer_values_are_rejected() -> Result<(), Box<dyn Error>> {
    let source = PdfFixture::malformed_large_trailer_values().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    Ok(())
}

#[tokio::test]
async fn escaped_encrypt_name_is_terminal_in_bounded_validation() -> Result<(), Box<dyn Error>> {
    let source = PdfFixture::large_escaped_encrypt_name()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::EncryptedOrLocked);
    Ok(())
}

#[tokio::test]
async fn nul_is_accepted_as_xref_whitespace() -> Result<(), Box<dyn Error>> {
    let source = PdfFixture::large_nul_xref_whitespace()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn metadata_reports_catalog_version_override() -> Result<(), Box<dyn Error>> {
    let fixture = PdfFixture::catalog_version_override()?;
    let expected_version = fixture.expected_version_override();
    let source = fixture.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;
    let body = complete_structure(result)?;

    assert_eq!(body["version"], expected_version);
    Ok(())
}

#[tokio::test]
async fn oversized_pdf_read_fails_at_the_declared_source_ceiling() -> Result<(), Box<dyn Error>> {
    let fixture = PdfFixture::over_source_limit()?;
    let expected_source_limit = fixture.expected_source_limit();
    let source = fixture.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await;

    assert!(source.byte_length().get() > expected_source_limit);
    assert_eq!(result, Err(FileMediaFailure::ProcessorFailed));
    Ok(())
}

#[tokio::test]
async fn compressed_content_bomb_fails_at_the_decode_ceiling() -> Result<(), Box<dyn Error>> {
    let source = PdfFixture::compressed_bomb()?.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await;

    assert_eq!(
        result,
        Err(FileMediaFailure::ExpansionLimitExceeded {
            limit_kind: ReasonCode::try_new("decoded_content_limit")?,
        })
    );
    Ok(())
}

#[tokio::test]
async fn recursive_content_reference_fails_without_partial_output() -> Result<(), Box<dyn Error>> {
    let source = PdfFixture::recursive_content_reference()?.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await;

    assert_eq!(result, Err(FileMediaFailure::ProcessorFailed));
    Ok(())
}

#[tokio::test]
async fn unknown_view_arguments_are_typed_and_content_silent() -> Result<(), Box<dyn Error>> {
    let source = PdfFixture::ordinary()?.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({"path": "../../host"}),
    )
    .await;

    assert_eq!(result, Err(FileMediaFailure::InvalidViewArguments));
    Ok(())
}

#[tokio::test]
async fn adversarial_decoder_text_is_rejected_by_registry_sanitization()
-> Result<(), Box<dyn Error>> {
    let source = PdfFixture::ordinary()?.into_source()?;
    let result = read(
        &AdversarialOutputProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await;

    assert_eq!(result, Err(FileMediaFailure::ProcessorFailed));
    Ok(())
}

fn registry() -> Result<FileMediaRegistry, Box<dyn Error>> {
    Ok(FileMediaRegistry::try_new(
        vec![declaration()?],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )?)
}

async fn inspect(
    processor: &dyn FileMediaProcessor,
    source: &MemorySource,
) -> Result<FileInspection, FileMediaFailure> {
    let request = InspectionRequest {
        source: source
            .file_use()
            .map_err(|_| FileMediaFailure::ProcessorFailed)?,
        visible_part: None,
    };
    registry()
        .map_err(|_| FileMediaFailure::ProcessorFailed)?
        .inspect(processor, request, source, &NeverCancelled)
        .await
}

async fn inspect_as(
    processor: &dyn FileMediaProcessor,
    source: &MemorySource,
    declared_media_type: &str,
) -> Result<FileInspection, FileMediaFailure> {
    let request = InspectionRequest {
        source: source
            .file_use_as(declared_media_type)
            .map_err(|_| FileMediaFailure::ProcessorFailed)?,
        visible_part: None,
    };
    registry()
        .map_err(|_| FileMediaFailure::ProcessorFailed)?
        .inspect(processor, request, source, &NeverCancelled)
        .await
}

async fn read(
    processor: &dyn FileMediaProcessor,
    source: &MemorySource,
    view: &str,
    options: serde_json::Value,
) -> Result<FileReadResult, FileMediaFailure> {
    let request = FileReadRequest {
        inspection: InspectionRequest {
            source: source
                .file_use()
                .map_err(|_| FileMediaFailure::ProcessorFailed)?,
            visible_part: None,
        },
        view: ReadViewName::try_new(view).map_err(|_| FileMediaFailure::ProcessorFailed)?,
        input: FileReadInput::Initial { options },
    };
    registry()
        .map_err(|_| FileMediaFailure::ProcessorFailed)?
        .read(processor, request, source, &NeverCancelled)
        .await
}

fn validated_view_count(inspection: &FileInspection) -> Result<usize, Box<dyn Error>> {
    match inspection {
        FileInspection::Validated(file) => Ok(file.views().len()),
        _ => Err("expected validated PDF".into()),
    }
}

fn malformed_reason(inspection: &FileInspection) -> Result<&str, Box<dyn Error>> {
    match inspection {
        FileInspection::Malformed { reason_code, .. } => Ok(reason_code.as_str()),
        _ => Err("expected malformed PDF".into()),
    }
}

fn complete_text(result: FileReadResult) -> Result<String, Box<dyn Error>> {
    match result {
        FileReadResult::Text {
            body,
            continuation: ReadContinuation::Complete,
        } => Ok(body),
        _ => Err("expected complete text result".into()),
    }
}

fn complete_structure(result: FileReadResult) -> Result<serde_json::Value, Box<dyn Error>> {
    match result {
        FileReadResult::Structured {
            body,
            continuation: ReadContinuation::Complete,
        } => Ok(body),
        _ => Err("expected complete structured result".into()),
    }
}
