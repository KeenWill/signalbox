mod fixtures;
mod support;

use std::error::Error;

use signalbox_file_media_runtime::{FileMediaFailure, ReasonCode};
use support::{DirectProcessor, MemorySource};

#[tokio::test]
async fn utf8_text_detects_validates_and_reads_exact_bytes() -> Result<(), Box<dyn Error>> {
    let bytes = fixtures::utf8_text();
    let expected = std::str::from_utf8(&bytes)?.to_owned();
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
    let result = support::read(&source, "text/plain", "text", &DirectProcessor::provider()).await?;
    support::assert_text(result, &expected);
    Ok(())
}

#[tokio::test]
async fn utf8_text_rejects_a_truncated_scalar_as_typed_malformed() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::truncated_utf8());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_malformed_reason(inspection, "invalid_utf8");
    Ok(())
}

#[tokio::test]
async fn utf8_text_rejects_oversized_input_with_registered_reason() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::oversized(b'a'));

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn json_detects_validates_and_returns_structured_data() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_document());
    let expected = fixtures::json_document_value();

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_validated_media(inspection, "application/json");
    let result = support::read(
        &source,
        "application/json",
        "structured",
        &DirectProcessor::provider(),
    )
    .await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn json_preserves_arbitrary_precision_numbers() -> Result<(), Box<dyn Error>> {
    let bytes = fixtures::arbitrary_precision_json();
    let expected = std::str::from_utf8(&bytes)?.to_owned();
    let source = MemorySource::new(bytes);

    let result = support::read(
        &source,
        "application/json",
        "structured",
        &DirectProcessor::provider(),
    )
    .await?;
    support::assert_structured_json(result, &expected);
    Ok(())
}

#[tokio::test]
async fn json_rejects_truncated_structure_as_typed_malformed() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::truncated_json());

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_malformed_reason(inspection, "malformed_json");
    Ok(())
}

#[tokio::test]
async fn json_rejects_oversized_input_with_registered_reason() -> Result<(), Box<dyn Error>> {
    let mut bytes = fixtures::oversized(b' ');
    bytes[0] = b'{';
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn pretty_json_is_not_ambiguous_with_csv() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::pretty_json_document());

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_validated_media(inspection, "application/json");
    Ok(())
}

#[tokio::test]
async fn bracket_prefixed_prose_uses_the_text_fallback() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::bracket_prefixed_prose());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
    Ok(())
}

#[tokio::test]
async fn invalid_utf8_streaming_text_candidate_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::truncated_utf8());

    let inspection = support::inspect(&source, "application/octet-stream").await?;
    support::assert_unknown(inspection);
    Ok(())
}

#[tokio::test]
async fn json_read_reports_the_declared_depth_limit() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_beyond_structured_depth());
    let expected = ReasonCode::try_new("depth_limit_exceeded")?;

    let result = support::read(
        &source,
        "application/json",
        "structured",
        &DirectProcessor::provider(),
    )
    .await;
    assert_eq!(
        result,
        Err(FileMediaFailure::ExpansionLimitExceeded {
            limit_kind: expected
        })
    );
    Ok(())
}

#[tokio::test]
async fn csv_detects_validates_and_returns_headers_and_rows() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_table());
    let expected = fixtures::csv_table_value();

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_validated_media(inspection, "text/csv");
    let result = support::read(
        &source,
        "text/csv",
        "structured",
        &DirectProcessor::provider(),
    )
    .await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn csv_rejects_truncated_quoted_field_as_typed_malformed() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::truncated_csv());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_malformed_reason(inspection, "malformed_csv");
    Ok(())
}

#[tokio::test]
async fn csv_rejects_quotes_inside_an_unquoted_field() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_with_quotes_inside_unquoted_field());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_malformed_reason(inspection, "malformed_csv");
    Ok(())
}

#[tokio::test]
async fn comma_bearing_prose_uses_the_text_fallback() -> Result<(), Box<dyn Error>> {
    let bytes = fixtures::prose_with_comma_and_newline();
    let expected = std::str::from_utf8(&bytes)?.to_owned();
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
    let result = support::read(&source, "text/plain", "text", &DirectProcessor::provider()).await?;
    support::assert_text(result, &expected);
    Ok(())
}

#[tokio::test]
async fn csv_rejects_row_bomb_shape_at_declared_ceiling() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::row_bomb_csv());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_malformed_reason(inspection, "row_limit_exceeded");
    Ok(())
}

#[tokio::test]
async fn csv_rejects_oversized_input_with_registered_reason() -> Result<(), Box<dyn Error>> {
    let mut bytes = fixtures::oversized(b'a');
    bytes[1] = b',';
    bytes[2] = b'\n';
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn registry_sanitizer_keeps_injection_shaped_json_as_data() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_document());
    let expected = serde_json::json!({
        "path":"../../etc/passwd",
        "text":"</tool><script>alert(1)</script>"
    });
    let decoder_output = serde_json::to_string(&expected)?;

    let result = support::read(
        &source,
        "application/json",
        "structured",
        &DirectProcessor::injecting(decoder_output),
    )
    .await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn registry_sanitizer_rejects_nul_bearing_decoder_output() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_document());
    let decoder_output = String::from("{\"text\":\"prefix\0suffix\"}");

    let result = support::read(
        &source,
        "application/json",
        "structured",
        &DirectProcessor::injecting(decoder_output),
    )
    .await;
    support::assert_processor_failed(result);
    Ok(())
}
