mod fixtures;

use std::error::Error;

use fixtures::{MemorySource, OfficeFixture};
use signalbox_file_media_adapter_office::{OfficeProvider, declaration};
use signalbox_file_media_runtime::{
    CancellationSignal, FileInspection, FileInspectionStatus, FileMediaCeilings, FileMediaFailure,
    FileMediaProcessor, FileMediaProcessorFuture, FileMediaProvider, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileMediaRegistry, FileReadRequest, FileReadResult,
    InspectionRequest, NeverCancelled, ProcessorIsolation, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ReadContinuation, ReadViewName, ReaderIdentity,
    VerifiedBlobSource,
};

struct DirectProcessor {
    provider: OfficeProvider,
}

impl DirectProcessor {
    const fn new() -> Self {
        Self {
            provider: OfficeProvider::new(),
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
        self.provider.probe(reader, source, cancellation)
    }

    fn validate<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        self.provider.inspect(reader, request, source, cancellation)
    }

    fn read<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderReadRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        self.provider.read(reader, request, source, cancellation)
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
            Ok(ProcessorReadOutput::Structured {
                body_json: String::from(r#"{"path":"../../host""#),
                truncated: false,
                cursor: None,
            })
        })
    }
}

#[test]
fn declaration_registers_three_macro_free_formats_under_available_isolation()
-> Result<(), Box<dyn Error>> {
    let registry = registry()?;

    assert_eq!(registry.providers(), &[declaration()?]);
    assert_eq!(declaration()?.readers().len(), 3);
    Ok(())
}

#[tokio::test]
async fn generated_docx_detects_and_extracts_embedded_text() -> Result<(), Box<dyn Error>> {
    assert_valid_text(OfficeFixture::docx()?).await
}

#[tokio::test]
async fn generated_xlsx_detects_and_extracts_embedded_text() -> Result<(), Box<dyn Error>> {
    assert_valid_text(OfficeFixture::xlsx()?).await
}

#[tokio::test]
async fn adjacent_spreadsheet_string_items_are_separated() -> Result<(), Box<dyn Error>> {
    let fixture = OfficeFixture::adjacent_shared_strings_xlsx()?;
    let expected_text = fixture.expected_text();
    let source = fixture.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await?;

    assert_eq!(complete_text(result)?, expected_text);
    Ok(())
}

#[tokio::test]
async fn generated_pptx_detects_and_extracts_embedded_text() -> Result<(), Box<dyn Error>> {
    assert_valid_text(OfficeFixture::pptx()?).await
}

#[tokio::test]
async fn presentation_relationship_order_controls_slide_text_order() -> Result<(), Box<dyn Error>> {
    let fixture = OfficeFixture::reordered_pptx()?;
    let expected_text = fixture.expected_text();
    let source = fixture.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await?;

    assert_eq!(complete_text(result)?, expected_text);
    Ok(())
}

#[tokio::test]
async fn generated_docx_metadata_reports_fixture_inventory() -> Result<(), Box<dyn Error>> {
    let fixture = OfficeFixture::docx()?;
    let expected_format = fixture.expected_format();
    let expected_entries = fixture.expected_entries();
    let source = fixture.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;
    let body = complete_structure(result)?;

    assert_eq!(body["format"], expected_format);
    assert_eq!(body["entries"], expected_entries);
    Ok(())
}

#[tokio::test]
async fn truncated_docx_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(OfficeFixture::truncated_docx()?).await
}

#[tokio::test]
async fn locked_docx_is_terminal_without_a_password_channel() -> Result<(), Box<dyn Error>> {
    let source = OfficeFixture::locked_docx()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::EncryptedOrLocked);
    Ok(())
}

#[tokio::test]
async fn macro_enabled_docx_is_not_accepted_as_macro_free_docx() -> Result<(), Box<dyn Error>> {
    assert_malformed(OfficeFixture::macro_enabled_docx()?).await
}

#[tokio::test]
async fn vba_part_is_rejected_despite_a_macro_free_main_override() -> Result<(), Box<dyn Error>> {
    assert_malformed(OfficeFixture::vba_part_in_macro_free_docx()?).await
}

#[tokio::test]
async fn package_with_docx_and_xlsx_main_parts_is_ambiguous() -> Result<(), Box<dyn Error>> {
    let source = OfficeFixture::mixed_docx_xlsx()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Ambiguous);
    Ok(())
}

#[tokio::test]
async fn zip_slip_shaped_entry_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(OfficeFixture::zip_slip_docx()?).await
}

#[tokio::test]
async fn recognized_zip_slip_docx_with_generic_declaration_remains_malformed()
-> Result<(), Box<dyn Error>> {
    let fixture = OfficeFixture::zip_slip_docx()?;
    let expected_reason = fixture.expected_reason()?;
    let source = fixture.into_unknown_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    assert_eq!(malformed_reason(&inspection)?, expected_reason);
    Ok(())
}

#[tokio::test]
async fn symlink_entry_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(OfficeFixture::symlink_docx()?).await
}

#[tokio::test]
async fn recursive_office_container_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>>
{
    assert_malformed(OfficeFixture::recursive_docx()?).await
}

#[tokio::test]
async fn compressed_expansion_bomb_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed(OfficeFixture::expansion_bomb_docx()?).await
}

#[tokio::test]
async fn large_opaque_office_part_does_not_consume_xml_expansion_budget()
-> Result<(), Box<dyn Error>> {
    assert_valid_text(OfficeFixture::large_opaque_part_docx()?).await
}

#[tokio::test]
async fn excessive_entry_count_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed(OfficeFixture::entry_count_bomb_docx()?).await
}

#[tokio::test]
async fn malformed_part_xml_fails_without_partial_output() -> Result<(), Box<dyn Error>> {
    let source = OfficeFixture::malformed_xml_docx()?.into_source()?;
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
async fn empty_part_xml_fails_without_partial_output() -> Result<(), Box<dyn Error>> {
    let source = OfficeFixture::empty_xml_docx()?.into_source()?;
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
async fn multiple_root_part_xml_fails_without_partial_output() -> Result<(), Box<dyn Error>> {
    let source = OfficeFixture::multiple_roots_docx()?.into_source()?;
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
async fn duplicate_office_part_name_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>>
{
    assert_malformed(OfficeFixture::duplicate_document_part_docx()?).await
}

#[tokio::test]
async fn oversized_extracted_text_is_a_typed_output_failure() -> Result<(), Box<dyn Error>> {
    let source = OfficeFixture::output_bomb_docx()?.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await;

    assert_eq!(result, Err(FileMediaFailure::OutputUnitTooLarge));
    Ok(())
}

#[tokio::test]
async fn unknown_bytes_remain_a_typed_unknown_inspection() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::unknown(b"not an Office container".to_vec())?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn hostile_view_arguments_are_typed_and_content_silent() -> Result<(), Box<dyn Error>> {
    let source = OfficeFixture::docx()?.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({"entry": "../../host"}),
    )
    .await;

    assert_eq!(result, Err(FileMediaFailure::InvalidViewArguments));
    Ok(())
}

#[tokio::test]
async fn adversarial_decoder_structure_is_rejected_by_registry_sanitization()
-> Result<(), Box<dyn Error>> {
    let source = OfficeFixture::docx()?.into_source()?;
    let result = read(
        &AdversarialOutputProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await;

    assert_eq!(result, Err(FileMediaFailure::ProcessorFailed));
    Ok(())
}

async fn assert_valid_text(fixture: OfficeFixture) -> Result<(), Box<dyn Error>> {
    let expected_text = fixture.expected_text();
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    assert!(complete_text(result)?.contains(expected_text));
    Ok(())
}

async fn assert_malformed(fixture: OfficeFixture) -> Result<(), Box<dyn Error>> {
    let expected_reason = fixture.expected_reason()?;
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    assert_eq!(malformed_reason(&inspection)?, expected_reason);
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
        options,
    };
    registry()
        .map_err(|_| FileMediaFailure::ProcessorFailed)?
        .read(processor, request, source, &NeverCancelled)
        .await
}

fn malformed_reason(inspection: &FileInspection) -> Result<&str, Box<dyn Error>> {
    match inspection {
        FileInspection::Malformed { reason_code, .. } => Ok(reason_code.as_str()),
        _ => Err("expected malformed Office container".into()),
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
