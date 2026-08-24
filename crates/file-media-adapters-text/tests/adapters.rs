mod fixtures;
mod support;

use std::error::Error;

use signalbox_file_media_runtime::{FileMediaCeilings, FileMediaFailure, ReasonCode};
use support::{DeclaredMismatchExpectation, DirectProcessor, MemorySource, ReadInput};

#[tokio::test]
async fn utf8_text_detects_validates_and_reads_exact_bytes() -> Result<(), Box<dyn Error>> {
    let bytes = fixtures::utf8_text();
    let expected = std::str::from_utf8(&bytes)?.to_owned();
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
    let result = support::read(
        &source,
        ReadInput {
            media_type: "text/plain",
            view: "text",
        },
        &DirectProcessor::provider(),
    )
    .await?;
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
        ReadInput {
            media_type: "application/json",
            view: "structured",
        },
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
        ReadInput {
            media_type: "application/json",
            view: "structured",
        },
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
async fn json_rejects_duplicate_object_members_as_malformed() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::duplicate_member_json());

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_malformed_reason(inspection, "malformed_json");
    Ok(())
}

#[tokio::test]
async fn json_rejects_duplicate_members_even_beyond_read_depth() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::deep_json_with_duplicate_root_member());

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_malformed_reason(inspection, "malformed_json");
    Ok(())
}

#[tokio::test]
async fn top_level_json_scalar_uses_the_text_fallback() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"true".to_vec());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
    Ok(())
}

#[tokio::test]
async fn unprobed_declared_json_candidate_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"hello".to_vec());

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_unknown(inspection);
    Ok(())
}

#[tokio::test]
async fn json_rejects_oversized_input_with_registered_reason() -> Result<(), Box<dyn Error>> {
    let mut bytes = fixtures::oversized(b' ');
    bytes[0] = b'{';
    bytes[1] = b'}';
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn json_honors_the_effective_validation_source_ceiling() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_document());
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 1;

    let inspection = support::inspect_with_ceilings(&source, "application/json", ceilings).await?;
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
async fn json_token_prefixed_prose_uses_the_text_fallback() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_token_prefixed_prose());

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
async fn json_at_the_declared_depth_limit_remains_readable() -> Result<(), Box<dyn Error>> {
    let bytes = fixtures::json_at_structured_depth();
    let expected = serde_json::from_slice(&bytes)?;
    let source = MemorySource::new(bytes);

    let result = support::read(
        &source,
        ReadInput {
            media_type: "application/json",
            view: "structured",
        },
        &DirectProcessor::provider(),
    )
    .await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn json_read_reports_the_declared_depth_limit() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_beyond_structured_depth());
    let expected = ReasonCode::try_new("depth_limit_exceeded")?;

    let result = support::read(
        &source,
        ReadInput {
            media_type: "application/json",
            view: "structured",
        },
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
async fn deeply_nested_valid_json_reports_the_declared_depth_limit() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_beyond_serde_recursion_limit());
    let expected = ReasonCode::try_new("depth_limit_exceeded")?;

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_validated_media(inspection, "application/json");
    let result = support::read(
        &source,
        ReadInput {
            media_type: "application/json",
            view: "structured",
        },
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
async fn bracketed_numeric_csv_is_not_ambiguous_with_json() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::bracketed_numeric_csv());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_validated_media(inspection, "text/csv");
    Ok(())
}

#[tokio::test]
async fn complete_json_array_records_are_not_ambiguous_with_csv() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::complete_json_arrays_as_csv());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_validated_media(inspection, "text/csv");
    Ok(())
}

#[tokio::test]
async fn complete_json_array_followed_by_prose_uses_text_fallback() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::complete_json_array_followed_by_prose());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
    Ok(())
}

#[tokio::test]
async fn complete_json_prefix_without_eof_uses_text_fallback() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::complete_json_prefix_followed_outside_probe());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
    Ok(())
}

#[tokio::test]
async fn json_probe_handles_a_utf8_scalar_split_at_its_boundary() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_with_scalar_split_at_probe_boundary());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_declared_mismatch(
        inspection,
        DeclaredMismatchExpectation {
            declared: "text/plain",
            detected: "application/json",
        },
    );
    Ok(())
}

#[tokio::test]
async fn deeply_nested_json_is_structurally_probed() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_beyond_serde_recursion_limit());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_declared_mismatch(
        inspection,
        DeclaredMismatchExpectation {
            declared: "text/plain",
            detected: "application/json",
        },
    );
    Ok(())
}

#[tokio::test]
async fn json_read_reports_the_container_entry_limit() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_beyond_container_entry_ceiling());
    let expected = ReasonCode::try_new("container_entry_limit_exceeded")?;

    let result = support::read(
        &source,
        ReadInput {
            media_type: "application/json",
            view: "structured",
        },
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
async fn json_read_honors_the_effective_container_entry_ceiling() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_document());
    let expected = ReasonCode::try_new("container_entry_limit_exceeded")?;
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.observed_container_entries = 2;

    let result = support::read_with_ceilings(
        &source,
        ReadInput {
            media_type: "application/json",
            view: "structured",
        },
        &DirectProcessor::provider(),
        ceilings,
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
async fn extremely_deep_json_reports_depth_limit_without_stack_walk() -> Result<(), Box<dyn Error>>
{
    let source = MemorySource::new(fixtures::deeply_nested_json_within_source_ceiling());
    let expected = ReasonCode::try_new("depth_limit_exceeded")?;

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_validated_media(inspection, "application/json");
    let result = support::read(
        &source,
        ReadInput {
            media_type: "application/json",
            view: "structured",
        },
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
        ReadInput {
            media_type: "text/csv",
            view: "structured",
        },
        &DirectProcessor::provider(),
    )
    .await?;
    support::assert_structured(result, &expected);
    Ok(())
}

#[tokio::test]
async fn declared_one_column_csv_validates() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::one_column_csv());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_validated_media(inspection, "text/csv");
    Ok(())
}

#[tokio::test]
async fn declared_header_only_csv_validates() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::header_only_csv());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_validated_media(inspection, "text/csv");
    Ok(())
}

#[tokio::test]
async fn malformed_quoted_csv_uses_the_text_fallback() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_with_quotes_inside_unquoted_field());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
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
async fn unprobed_declared_csv_candidate_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"hello".to_vec());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_unknown(inspection);
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
async fn csv_rejects_a_blank_record_as_typed_malformed() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_with_blank_record());

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
    let result = support::read(
        &source,
        ReadInput {
            media_type: "text/plain",
            view: "text",
        },
        &DirectProcessor::provider(),
    )
    .await?;
    support::assert_text(result, &expected);
    Ok(())
}

#[tokio::test]
async fn csv_probe_ignores_a_partial_trailing_record() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_with_partial_third_probe_record());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_declared_mismatch(
        inspection,
        DeclaredMismatchExpectation {
            declared: "text/plain",
            detected: "text/csv",
        },
    );
    Ok(())
}

#[tokio::test]
async fn csv_probe_rejects_a_partial_second_record() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_with_partial_second_probe_record());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
    Ok(())
}

#[tokio::test]
async fn csv_probe_handles_a_utf8_scalar_split_at_its_boundary() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_with_scalar_split_at_probe_boundary());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_declared_mismatch(
        inspection,
        DeclaredMismatchExpectation {
            declared: "text/plain",
            detected: "text/csv",
        },
    );
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
    bytes[..8].copy_from_slice(b"a,b\nc,d\n");
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn oversized_declared_one_column_csv_preserves_the_size_reason() -> Result<(), Box<dyn Error>>
{
    let source = MemorySource::new(fixtures::oversized_one_column_csv());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn csv_read_honors_the_effective_container_entry_ceiling() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_table());
    let expected = ReasonCode::try_new("container_entry_limit_exceeded")?;
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.observed_container_entries = 1;

    let result = support::read_with_ceilings(
        &source,
        ReadInput {
            media_type: "text/csv",
            view: "structured",
        },
        &DirectProcessor::provider(),
        ceilings,
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
async fn registry_sanitizer_keeps_injection_shaped_json_as_data() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_document());
    let expected = serde_json::json!({
        "path":"../../etc/passwd",
        "text":"</tool><script>alert(1)</script>"
    });
    let decoder_output = serde_json::to_string(&expected)?;

    let result = support::read(
        &source,
        ReadInput {
            media_type: "application/json",
            view: "structured",
        },
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
        ReadInput {
            media_type: "application/json",
            view: "structured",
        },
        &DirectProcessor::injecting(decoder_output),
    )
    .await;
    support::assert_processor_failed(result);
    Ok(())
}
