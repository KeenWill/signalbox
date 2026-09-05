//! Contract tests for the data-only SVG adapter.
//! Governed by `docs/spec/file-and-media.md`.

mod fixtures;

use std::error::Error;

use fixtures::{MemorySource, SvgFixture};
use signalbox_file_media_adapter_svg::{SvgProvider, declaration};
use signalbox_file_media_runtime::{
    CancellationSignal, FileInspection, FileInspectionStatus, FileMediaCeilings, FileMediaFailure,
    FileMediaProcessor, FileMediaProcessorFuture, FileMediaProvider, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileMediaRegistry, FileReadInput, FileReadRequest,
    FileReadResult, InspectionRequest, NeverCancelled, ProcessorBoundaryFailure, ProcessorFailure,
    ProcessorIsolation, ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput,
    ReadContinuation, ReadViewName, ReaderIdentity, VerifiedBlobSource,
};

macro_rules! assert_malformed {
    ($fixture:expr, $expected_reason:expr $(,)?) => {
        async {
            let (status, reason) = malformed_observation($fixture).await?;
            assert_eq!(status, FileInspectionStatus::Malformed);
            assert_eq!(reason, $expected_reason);
            Ok::<(), Box<dyn Error>>(())
        }
    };
}

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
fn declaration_registers_data_only_svg_under_available_isolation() -> Result<(), Box<dyn Error>> {
    let registry = registry()?;

    assert_eq!(registry.providers(), &[declaration()?]);
    Ok(())
}

#[tokio::test]
async fn generated_svg_validates() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::ordinary().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn generated_svg_extracts_text() -> Result<(), Box<dyn Error>> {
    let fixture = SvgFixture::ordinary();
    let expected_text = fixture.expected_text();
    let source = fixture.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "text",
        serde_json::json!({}),
    )
    .await?;

    assert!(complete_text(result)?.contains(expected_text));
    Ok(())
}

#[tokio::test]
async fn empty_text_element_matches_explicit_start_and_end() -> Result<(), Box<dyn Error>> {
    let processor = DirectProcessor::new();
    let empty = SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"><text/></svg>"#)
        .into_source()?;
    let explicit =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"><text></text></svg>"#)
            .into_source()?;

    let empty_text = complete_text(read(&processor, &empty, "text", serde_json::json!({})).await?)?;
    let explicit_text =
        complete_text(read(&processor, &explicit, "text", serde_json::json!({})).await?)?;

    assert_eq!(empty_text, explicit_text);
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
    assert_malformed!(SvgFixture::truncated(), "malformed_svg").await
}

#[tokio::test]
async fn invalid_utf8_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed!(SvgFixture::invalid_utf8(), "malformed_svg").await
}

#[tokio::test]
async fn forbidden_xml_character_in_ordinary_text_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(b"<svg xmlns=\"http://www.w3.org/2000/svg\"><text>a\x01b</text></svg>",),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn utf16_little_endian_svg_is_accepted() -> Result<(), Box<dyn Error>> {
    let xml = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><svg xmlns=\"http://www.w3.org/2000/svg\"><text>ok</text></svg>";
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    let source = SvgFixture::raw(&bytes).into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn bomless_generic_utf16_is_rejected() -> Result<(), Box<dyn Error>> {
    let xml =
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>";
    let bytes: Vec<u8> = xml.encode_utf16().flat_map(u16::to_le_bytes).collect();

    assert_malformed!(SvgFixture::raw(&bytes), "malformed_svg").await
}

#[tokio::test]
async fn utf16_svg_without_declared_encoding_is_accepted() -> Result<(), Box<dyn Error>> {
    let xml = "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>";
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    let source = SvgFixture::raw(&bytes).into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn utf16_big_endian_svg_is_accepted() -> Result<(), Box<dyn Error>> {
    let xml = "<?xml version=\"1.0\" encoding=\"UTF-16BE\"?><svg xmlns=\"http://www.w3.org/2000/svg\"><text>ok</text></svg>";
    let mut bytes = vec![0xfe, 0xff];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_be_bytes));
    let source = SvgFixture::raw(&bytes).into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn probe_preserves_utf16_root_before_invalid_surrogate() -> Result<(), Box<dyn Error>> {
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend(
        r#"<svg xmlns="http://www.w3.org/2000/svg">"#
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    bytes.extend_from_slice(&0xdc00_u16.to_le_bytes());
    bytes.extend("</svg>".encode_utf16().flat_map(u16::to_le_bytes));
    let source = SvgFixture::raw(&bytes).into_source()?;
    let inspection =
        inspect_as(&DirectProcessor::new(), &source, "application/octet-stream").await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    Ok(())
}

#[tokio::test]
async fn entity_expansion_shape_is_rejected_before_expansion() -> Result<(), Box<dyn Error>> {
    assert_malformed!(SvgFixture::entity_bomb(), "malformed_svg").await
}

#[tokio::test]
async fn script_is_rejected_as_active_content() -> Result<(), Box<dyn Error>> {
    assert_malformed!(SvgFixture::script(), "active_content").await
}

#[tokio::test]
async fn external_image_is_rejected_without_resource_fetching() -> Result<(), Box<dyn Error>> {
    assert_malformed!(SvgFixture::external_image(), "external_reference").await
}

#[tokio::test]
async fn nested_svg_is_rejected_as_a_recursive_container() -> Result<(), Box<dyn Error>> {
    assert_malformed!(SvgFixture::nested_svg(), "nested_svg").await
}

#[tokio::test]
async fn excessive_element_count_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed!(SvgFixture::excessive_elements(), "structure_limit").await
}

#[tokio::test]
async fn self_closing_element_counts_against_depth_limit() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::empty_child_beyond_depth_limit(),
        "structure_limit",
    )
    .await
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
    assert_malformed!(SvgFixture::oversized_source(), "source_size_limit").await
}

#[tokio::test]
async fn oversized_non_svg_source_is_unknown() -> Result<(), Box<dyn Error>> {
    let mut bytes = b"<foo>".to_vec();
    bytes.resize(256 * 1024 + 1, b'a');
    let source = SvgFixture::raw(&bytes).into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Unknown
    );
    Ok(())
}

#[tokio::test]
async fn lowered_validation_ceiling_is_a_typed_validation_limit() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::ordinary().into_source()?;
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 64;
    let request = InspectionRequest {
        source: source
            .file_use()
            .map_err(|_| FileMediaFailure::ProcessorFailed)?,
        visible_part: None,
    };
    let inspection = registry_with(ceilings)?
        .inspect(&DirectProcessor::new(), request, &source, &NeverCancelled)
        .await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    assert_eq!(malformed_reason(&inspection)?, "source_size_limit");
    Ok(())
}

#[tokio::test]
async fn truncated_root_probe_under_a_lowered_ceiling_is_a_typed_validation_limit()
-> Result<(), Box<dyn Error>> {
    // A legitimate SVG whose root start tag is long enough that a very low
    // `validation_source_bytes` ceiling cuts the prefix probe off mid-tag.
    // The unbounded top-level probe (bounded only by `PROBE_BYTES`) still
    // sees the whole tag and classifies this as a structural SVG candidate,
    // so the truncated re-probe inside `inspect` must not report `NoMatch`
    // for what is really an indeterminate, not a disproven, root: doing so
    // would turn a typed `source_size_limit` outcome into a hard processor
    // failure for oversized-but-genuine SVG content.
    let mut bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" data-pad=""#.to_vec();
    bytes.extend(std::iter::repeat_n(b'A', 300));
    bytes.extend_from_slice(b"\">");
    bytes.resize(256 * 1024 + 1, b' ');
    bytes.extend_from_slice(b"</svg>");
    let source = SvgFixture::raw(&bytes).into_source()?;
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 100;
    let request = InspectionRequest {
        source: source
            .file_use()
            .map_err(|_| FileMediaFailure::ProcessorFailed)?,
        visible_part: None,
    };
    let inspection = registry_with(ceilings)?
        .inspect(&DirectProcessor::new(), request, &source, &NeverCancelled)
        .await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    assert_eq!(malformed_reason(&inspection)?, "source_size_limit");
    Ok(())
}

#[tokio::test]
async fn non_svg_source_exceeding_only_the_lowered_ceiling_is_unknown() -> Result<(), Box<dyn Error>>
{
    // Declared as SVG but not actually SVG, and only over the deployment's
    // lowered `validation_source_bytes` ceiling, not the adapter's hard
    // ceiling. Bounded root classification must still run so the registry
    // reports the ordinary `Unknown` a declared-but-unvalidated candidate
    // gets, rather than a misleading `Malformed`/`source_size_limit`.
    let mut bytes = b"<foo/>".to_vec();
    bytes.resize(50, b'a');
    let source = SvgFixture::raw(&bytes).into_source()?;
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 10;
    let request = InspectionRequest {
        source: source
            .file_use()
            .map_err(|_| FileMediaFailure::ProcessorFailed)?,
        visible_part: None,
    };
    let inspection = registry_with(ceilings)?
        .inspect(&DirectProcessor::new(), request, &source, &NeverCancelled)
        .await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn malformed_dimension_is_rejected_before_metadata_output() -> Result<(), Box<dyn Error>> {
    assert_malformed!(SvgFixture::malformed_dimension(), "malformed_svg").await
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
    assert_malformed!(
        SvgFixture::raw(br#"<evil:svg xmlns="http://www.w3.org/2000/svg" xmlns:evil="urn:evil"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn foreign_namespaced_svg_root_is_unknown_without_svg_declaration()
-> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(br#"<x:svg xmlns:x="urn:example"/>"#).into_source()?;
    let inspection =
        inspect_as(&DirectProcessor::new(), &source, "application/octet-stream").await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn foreign_namespaced_svg_root_is_unknown_with_svg_declaration() -> Result<(), Box<dyn Error>>
{
    let source = SvgFixture::raw(br#"<x:svg xmlns:x="urn:example"/>"#).into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn plain_text_is_unknown_with_svg_declaration() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(b"not SVG").into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn invalid_utf8_after_foreign_svg_root_is_unknown() -> Result<(), Box<dyn Error>> {
    let mut bytes = br#"<x:svg xmlns:x="urn:other"/>"#.to_vec();
    bytes.push(0xff);
    let source = SvgFixture::raw(&bytes).into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Unknown
    );
    Ok(())
}

#[tokio::test]
async fn unnamespaced_non_svg_root_is_unknown_with_svg_declaration() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(br#"<foo/>"#).into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn invalid_utf8_after_non_svg_root_is_unknown() -> Result<(), Box<dyn Error>> {
    let mut bytes = b"<foo>".to_vec();
    bytes.push(0xff);
    bytes.extend_from_slice(b"</foo>");
    let source = SvgFixture::raw(&bytes).into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Unknown
    );
    Ok(())
}

#[tokio::test]
async fn processing_instruction_before_non_svg_root_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(br#"<?audit?><foo/>"#).into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn leading_text_before_non_svg_root_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(br#"junk<foo/>"#).into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn leading_text_before_svg_root_is_recognized_as_malformed() -> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"junk<svg xmlns="http://www.w3.org/2000/svg"/>"#).into_source()?;
    let inspection =
        inspect_as(&DirectProcessor::new(), &source, "application/octet-stream").await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    assert_eq!(malformed_reason(&inspection)?, "malformed_svg");
    Ok(())
}

#[tokio::test]
async fn invalid_declaration_before_non_svg_root_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(br#"<?xml?><foo/>"#).into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn dtd_bearing_svg_is_malformed_without_svg_declaration() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(br#"<!DOCTYPE svg><svg xmlns="http://www.w3.org/2000/svg"/>"#)
        .into_source()?;
    let inspection =
        inspect_as(&DirectProcessor::new(), &source, "application/octet-stream").await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    assert_eq!(malformed_reason(&inspection)?, "malformed_svg");
    Ok(())
}

#[tokio::test]
async fn animation_element_is_rejected_as_active_content() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><animate attributeName="x"/></svg>"#,
        ),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn color_animation_element_is_rejected_as_active_content() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><animateColor attributeName="fill" from="red" to="blue"/></svg>"#,
        ),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn foreign_namespaced_script_is_rejected_as_active_content() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:h="http://www.w3.org/1999/xhtml"><h:script>run()</h:script></svg>"#,
        ),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn foreign_resource_element_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:h="http://www.w3.org/1999/xhtml"><h:img src="https://example.test/a.png"/></svg>"#,
        ),
        "external_reference",
    )
    .await
}

#[tokio::test]
async fn foreign_input_source_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:h="http://www.w3.org/1999/xhtml"><h:input type="image" src="https://example.test/a.png"/></svg>"#,
        ),
        "external_reference",
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
async fn top_level_cdata_before_non_svg_root_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(br#"<![CDATA[x]]><foo/>"#).into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn forbidden_xml_character_in_cdata_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(b"<svg xmlns=\"http://www.w3.org/2000/svg\"><![CDATA[a\x01b]]></svg>",),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn trailing_document_entity_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"/>&amp;"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn malformed_view_box_separator_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0,,0,320,200"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn invalid_view_box_number_token_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1. 2"/>"#,),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn zero_sized_view_box_is_valid_metadata() -> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 0 100"/>"#)
            .into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;

    assert_eq!(
        complete_structure(result)?["view_box"],
        serde_json::json!([0.0, 0.0, 0.0, 100.0])
    );
    Ok(())
}

#[tokio::test]
async fn escaped_css_resource_reference_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><path fill="u\72 l(https://example.invalid/x)"/></svg>"#,
        ),
        "external_reference",
    )
    .await
}

#[tokio::test]
async fn offset_path_resource_reference_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><path offset-path="url(https://example.invalid/path.svg#p)"/></svg>"#,
        ),
        "external_reference",
    )
    .await
}

#[tokio::test]
async fn color_profile_resource_reference_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><rect color-profile="url(https://example.invalid/profile.icc)"/></svg>"#,
        ),
        "external_reference",
    )
    .await
}

#[tokio::test]
async fn declaration_after_comment_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<!--before--><?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"/>"#,
        ),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn forbidden_attribute_control_character_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(b"<svg xmlns=\"http://www.w3.org/2000/svg\" aria-label=\"a\x01b\"/>"),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn dimension_with_trailing_decimal_point_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="1.px"/>"#,),
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
async fn modern_relative_dimension_units_are_valid_without_numeric_metadata()
-> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="1rem" height="2dvh"/>"#)
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
async fn auto_and_container_dimensions_are_valid_without_numeric_metadata()
-> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="auto" height="1cqw"/>"#)
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
async fn css_wide_dimension_keywords_are_valid_without_numeric_metadata()
-> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="inherit" height="REVERT-LAYER"/>"#,
    )
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
async fn calculated_dimensions_are_valid_without_numeric_metadata() -> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(100% - 1px)"/>"#)
            .into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;

    assert_eq!(
        complete_structure(result)?["width"],
        serde_json::Value::Null
    );
    Ok(())
}

#[tokio::test]
async fn calculation_products_are_valid_without_numeric_metadata() -> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(2 * 10px)"/>"#)
            .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn calculation_allows_negative_dimension_intermediates() -> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(-1px + 2px)"/>"#)
            .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn negative_constant_calculation_result_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(-1px)"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn negative_constant_non_pixel_calculations_are_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(-1cm)"/>"#),
        "malformed_svg",
    )
    .await?;
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(-1%)"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn clamp_minimum_precedes_inverted_maximum() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="clamp(1px, 2px, -1px)"/>"#,
    )
    .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn negative_mixed_absolute_unit_calculation_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(-1cm + 1mm)"/>"#,),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn division_by_zero_calculation_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(1px / 0)"/>"#,),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn calculated_e_prefixed_unit_is_accepted() -> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(1em + 2px)"/>"#)
            .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn calculation_function_names_are_ascii_case_insensitive() -> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="CALC(1px + 2px)"/>"#)
            .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn invalid_calculation_dimension_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(banana)"/>"#,),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn calculation_function_name_must_touch_opening_parenthesis() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc (1px)"/>"#),
        "malformed_svg",
    )
    .await?;
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="min (1px)"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn negative_zero_calculation_dimension_is_accepted() -> Result<(), Box<dyn Error>> {
    let source =
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(-0px)"/>"#)
            .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn calculation_treats_css_comments_as_whitespace() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(1px /* gap */ + 2px)"/>"#,
    )
    .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn calculation_addition_requires_surrounding_whitespace() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(1px+2px)"/>"#,),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn empty_calculation_arguments_are_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="min(,)"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn dimensions_admit_surrounding_xml_whitespace() -> Result<(), Box<dyn Error>> {
    let fixture = SvgFixture::dimensions_with_surrounding_xml_whitespace();
    let expected_width = fixture.expected_whitespace_width();
    let expected_height = fixture.expected_whitespace_height();
    let source = fixture.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;
    let body = complete_structure(result)?;

    assert_eq!(body["width"], expected_width);
    assert_eq!(body["height"], expected_height);
    Ok(())
}

#[tokio::test]
async fn non_xml_whitespace_outside_root_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw("\u{00a0}<svg xmlns=\"http://www.w3.org/2000/svg\"/>".as_bytes()),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn prolog_processing_instruction_is_active_content() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<?xml-stylesheet href="x.css"?><svg xmlns="http://www.w3.org/2000/svg"/>"#,
        ),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn invalid_xml_comment_syntax_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"><!--a--b--></svg>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn xml_comment_body_ending_in_hyphen_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"><!--a---></svg>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn forbidden_xml_character_in_comment_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(b"<svg xmlns=\"http://www.w3.org/2000/svg\"><!--a\x01b--></svg>",),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn incomplete_xml_declaration_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<?xml?><svg xmlns="http://www.w3.org/2000/svg"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn vertical_tab_in_xml_declaration_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(b"<?xml\x0bversion=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>",),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn unbound_descendant_prefix_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
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
async fn signed_numeric_character_reference_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"><text>&#+65;</text></svg>"#,),
        "malformed_svg",
    )
    .await
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
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" onclick="run()"/>"#),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn svg_handler_element_is_rejected_as_active_content() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:ev="http://www.w3.org/2001/xml-events"><handler type="application/ecmascript" ev:event="click">alert(1)</handler></svg>"#,
        ),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn root_window_event_handler_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" onbeforeunload="run()"/>"#,),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn unbound_attribute_prefix_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" p:x="1"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn reserved_xml_prefix_rebinding_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xml="urn:evil"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn invalid_namespace_iri_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:p="urn:bad value"><p:path/></svg>"#,
        ),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn invalid_namespace_iri_authority_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:p="http://["><p:path/></svg>"#,
        ),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn duplicate_expanded_attribute_name_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:p="urn:x" xmlns:q="urn:x" p:a="1" q:a="2"/>"#,
        ),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn foreign_namespaced_width_does_not_change_metadata() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(
        br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:ext="urn:example" width="10" ext:width="999"/>"#,
    )
    .into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;
    let body = complete_structure(result)?;

    assert_eq!(body["width"], 10.0);
    Ok(())
}

#[tokio::test]
async fn namespace_character_references_are_expanded_before_policy_checks()
-> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(
            br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:x="http://www.w3.org/1999/xlin&#x6b;" x:href="https://example.invalid/a.svg"/>"#,
        ),
        "external_reference",
    )
    .await
}

#[tokio::test]
async fn url_text_in_inert_attribute_is_accepted() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(
        br#"<svg xmlns="http://www.w3.org/2000/svg" aria-label="Use url(example)"/>"#,
    )
    .into_source()?;

    assert_eq!(
        inspect(&DirectProcessor::new(), &source).await?.status(),
        FileInspectionStatus::Validated
    );
    Ok(())
}

#[tokio::test]
async fn context_menu_event_handler_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" oncontextmenu="run()"/>"#),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn auxiliary_click_event_handler_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" onauxclick="run()"/>"#),
        "active_content",
    )
    .await
}

#[tokio::test]
async fn invalid_descendant_element_name_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"><1path/></svg>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn forbidden_character_data_terminator_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg"><text>a]]>b</text></svg>"#),
        "malformed_svg",
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
async fn probe_accepts_root_before_truncated_utf8_character() -> Result<(), Box<dyn Error>> {
    let mut bytes = br#"<svg xmlns="http://www.w3.org/2000/svg"><text>"#.to_vec();
    bytes.resize(65_535, b'a');
    bytes.extend_from_slice("é</text></svg>".as_bytes());
    let source = SvgFixture::raw(&bytes).into_source()?;
    let inspection =
        inspect_as(&DirectProcessor::new(), &source, "application/octet-stream").await?;

    assert_ne!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn probe_recognizes_root_before_invalid_utf8() -> Result<(), Box<dyn Error>> {
    let mut bytes = br#"<svg xmlns="http://www.w3.org/2000/svg">"#.to_vec();
    bytes.push(0xff);
    bytes.extend_from_slice(b"</svg>");
    let source = SvgFixture::raw(&bytes).into_source()?;
    let inspection =
        inspect_as(&DirectProcessor::new(), &source, "application/octet-stream").await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
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
    registry_with(FileMediaCeilings::version_one())
}

fn registry_with(ceilings: FileMediaCeilings) -> Result<FileMediaRegistry, Box<dyn Error>> {
    Ok(FileMediaRegistry::try_new(
        vec![declaration()?],
        ceilings,
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
        input: FileReadInput::Initial { options },
    };
    registry()
        .map_err(|_| FileMediaFailure::ProcessorFailed)?
        .read(processor, request, source, &NeverCancelled)
        .await
}

async fn malformed_observation(
    fixture: SvgFixture,
) -> Result<(FileInspectionStatus, String), Box<dyn Error>> {
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;
    let status = inspection.status();
    let reason = String::from(malformed_reason(&inspection)?);
    Ok((status, reason))
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

#[tokio::test]
async fn view_box_trailing_comma_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 320 200,"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn angle_dimension_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="1deg"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn time_dimension_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="2s"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn unknown_dimension_unit_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="10bananas"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn clamp_requires_three_arguments() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="clamp(1px)"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn calculation_unterminated_trailing_comment_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed!(
        SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc(1px)/*"/>"#),
        "malformed_svg",
    )
    .await
}

#[tokio::test]
async fn uppercase_pixel_unit_retains_numeric_metadata() -> Result<(), Box<dyn Error>> {
    let source = SvgFixture::raw(br#"<svg xmlns="http://www.w3.org/2000/svg" width="10PX"/>"#)
        .into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;
    assert_eq!(
        complete_structure(result)?["width"],
        serde_json::json!(10.0)
    );
    Ok(())
}
