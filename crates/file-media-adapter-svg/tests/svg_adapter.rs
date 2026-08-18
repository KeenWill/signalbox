mod fixtures;

use std::error::Error;

use fixtures::{MemorySource, SvgFixture};
use signalbox_file_media_adapter_svg::{SvgProvider, declaration};
use signalbox_file_media_runtime::{
    CancellationSignal, FileInspection, FileInspectionStatus, FileMediaCeilings, FileMediaFailure,
    FileMediaProcessor, FileMediaProcessorFuture, FileMediaProvider, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileMediaRegistry, FileReadRequest, FileReadResult,
    InspectionRequest, NeverCancelled, ProcessorIsolation, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ReadContinuation, ReadViewName, ReaderIdentity,
    VerifiedBlobSource,
};

struct DirectProcessor {
    provider: SvgProvider,
}

impl DirectProcessor {
    const fn new() -> Self {
        Self {
            provider: SvgProvider::new(),
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
            Ok(ProcessorReadOutput::Text {
                body: String::from("decoder\0injection"),
                truncated: false,
                cursor: None,
            })
        })
    }
}

#[test]
fn declaration_registers_data_only_svg_under_available_isolation() -> Result<(), Box<dyn Error>> {
    let registry = registry()?;

    assert_eq!(registry.providers(), &[declaration()?]);
    Ok(())
}

#[tokio::test]
async fn generated_svg_validates_and_extracts_text() -> Result<(), Box<dyn Error>> {
    let fixture = SvgFixture::ordinary();
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

#[tokio::test]
async fn generated_svg_metadata_reports_fixture_shape() -> Result<(), Box<dyn Error>> {
    let fixture = SvgFixture::ordinary();
    let expected_elements = fixture.expected_elements();
    let expected_width = fixture.expected_width();
    let expected_height = fixture.expected_height();
    let expected_view_box = fixture.expected_view_box();
    let source = fixture.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;
    let body = complete_structure(result)?;

    assert_eq!(body["elements"], expected_elements);
    assert_eq!(body["width"], expected_width);
    assert_eq!(body["height"], expected_height);
    assert_eq!(body["view_box"], serde_json::json!(expected_view_box));
    Ok(())
}

#[tokio::test]
async fn truncated_svg_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(SvgFixture::truncated(), "malformed_svg").await
}

#[tokio::test]
async fn invalid_utf8_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(SvgFixture::invalid_utf8(), "malformed_svg").await
}

#[tokio::test]
async fn entity_expansion_shape_is_rejected_before_expansion() -> Result<(), Box<dyn Error>> {
    assert_malformed(SvgFixture::entity_bomb(), "malformed_svg").await
}

#[tokio::test]
async fn script_is_rejected_as_active_content() -> Result<(), Box<dyn Error>> {
    assert_malformed(SvgFixture::script(), "active_content").await
}

#[tokio::test]
async fn external_image_is_rejected_without_resource_fetching() -> Result<(), Box<dyn Error>> {
    assert_malformed(SvgFixture::external_image(), "external_reference").await
}

#[tokio::test]
async fn nested_svg_is_rejected_as_a_recursive_container() -> Result<(), Box<dyn Error>> {
    assert_malformed(SvgFixture::nested_svg(), "nested_svg").await
}

#[tokio::test]
async fn excessive_element_count_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed(SvgFixture::excessive_elements(), "structure_limit").await
}

#[tokio::test]
async fn excessive_text_is_a_typed_output_failure() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::output_bomb().into_source()?;
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
async fn oversized_source_is_a_typed_validation_limit() -> Result<(), Box<dyn Error>> {
    assert_malformed(SvgFixture::oversized_source(), "source_size_limit").await
}

#[tokio::test]
async fn malformed_dimension_is_rejected_before_metadata_output() -> Result<(), Box<dyn Error>> {
    assert_malformed(SvgFixture::malformed_dimension(), "malformed_svg").await
}

#[tokio::test]
async fn prefixed_svg_namespace_is_accepted() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(
        br#"<s:svg xmlns:s="http://www.w3.org/2000/svg"><s:text>ok</s:text></s:svg>"#,
    )
    .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn foreign_prefixed_svg_root_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(br#"<evil:svg xmlns="http://www.w3.org/2000/svg" xmlns:evil="urn:evil"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn animation_element_is_rejected_as_active_content() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><animate attributeName="x"/></svg>"#,
        ),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn built_in_attribute_entity_is_accepted() -> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" aria-label="A &amp; B"/>"#)
            .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn cdata_in_text_is_extracted_as_inert_text() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(
        br#"<svg xmlns="http://www.w3.org/2000/svg"><text><![CDATA[a < b]]></text></svg>"#,
    )
    .into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await?;

    assert_eq!(complete_text(result)?, "a < b\n");
    Ok(())
}

#[tokio::test]
async fn trailing_document_entity_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"/>&amp;"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn malformed_view_box_separator_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0,,0,320,200"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn escaped_css_resource_reference_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><path fill="u\72 l(https://example.invalid/x)"/></svg>"#,
        ),
        "external_reference",
    )
    .await
}

#[tokio::test]
async fn declaration_after_comment_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(
            br#"<!--before--><?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"/>"#,
        ),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn forbidden_attribute_control_character_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(b"<svg xmlns=\"http://www.w3.org/2000/svg\" aria-label=\"a\x01b\"/>"),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn relative_dimension_units_are_valid_without_numeric_metadata() -> Result<(), Box<dyn Error>>
{
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="10cm"/>"#)
            .into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;
    let body = complete_structure(result)?;

    assert_eq!(body["width"], serde_json::Value::Null);
    assert_eq!(body["height"], serde_json::Value::Null);
    Ok(())
}

#[tokio::test]
async fn invalid_xml_comment_syntax_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"><!--a--b--></svg>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn incomplete_xml_declaration_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(br#"<?xml?><svg xmlns="http://www.w3.org/2000/svg"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn unbound_descendant_prefix_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"><p:path/></svg>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn numeric_character_references_are_extracted() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(
        br#"<svg xmlns="http://www.w3.org/2000/svg"><text>&#65;&#x42;</text></svg>"#,
    )
    .into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await?;

    assert_eq!(complete_text(result)?, "AB\n");
    Ok(())
}

#[tokio::test]
async fn harmless_on_prefixed_names_are_accepted() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(
        br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:only="urn:example" only:once="yes"/>"#,
    )
    .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn actual_event_handler_attribute_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" onclick="run()"/>"#),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn unknown_bytes_remain_a_typed_unknown_inspection() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::unknown(b"not SVG".to_vec())?;
    let inspection =
        inspect_as(&DirectProcessor::new(), &source, "application/octet-stream").await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn hostile_view_arguments_are_typed_and_content_silent() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::ordinary().into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({"resource": "../../host"}),
    )
    .await;

    assert_eq!(result, Err(FileMediaFailure::InvalidViewArguments));
    Ok(())
}

#[tokio::test]
async fn adversarial_decoder_text_is_rejected_by_registry_sanitization()
-> Result<(), Box<dyn Error>> {
    let source = SvgFixture::ordinary().into_source()?;
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
    inspect_as(processor, source, "image/svg+xml").await
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
        options,
    };
    registry()
        .map_err(|_| FileMediaFailure::ProcessorFailed)?
        .read(processor, request, source, &NeverCancelled)
        .await
}

async fn assert_malformed(fixture: SvgFixture, reason: &str) -> Result<(), Box<dyn Error>> {
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    assert_eq!(malformed_reason(&inspection)?, reason);
    Ok(())
}

fn malformed_reason(inspection: &FileInspection) -> Result<&str, Box<dyn Error>> {
    match inspection {
        FileInspection::Malformed { reason_code, .. } => Ok(reason_code.as_str()),
        _ => Err("expected malformed SVG".into()),
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
