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
    assert!(
        matches!(&inspection, signalbox_file_media_runtime::FileInspection::Validated(file)
        if file.validation() == signalbox_file_media_runtime::ValidationEvidence::StreamingTextValidation)
    );
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
async fn truncated_utf8_text_remains_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::truncated_utf8());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_unknown(inspection);
    Ok(())
}

#[tokio::test]
async fn complete_source_json_probe_does_not_drop_invalid_utf8_suffix() -> Result<(), Box<dyn Error>>
{
    let source = MemorySource::new(vec![b'{', b'}', 0xc3]);

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_unknown(inspection);
    Ok(())
}

#[tokio::test]
async fn oversized_streaming_text_inspects_a_bounded_prefix() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::oversized(b'a'));

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
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
async fn unprobed_declared_json_candidate_uses_text_fallback() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"hello".to_vec());

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_declared_mismatch(
        inspection,
        DeclaredMismatchExpectation {
            declared: "application/json",
            detected: "text/plain",
        },
    );
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
async fn oversized_declared_json_scalar_preserves_the_size_reason() -> Result<(), Box<dyn Error>> {
    let mut bytes = b"true".to_vec();
    bytes.resize(128 * 1_024 + 1, b' ');
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn oversized_declared_json_string_preserves_the_size_reason() -> Result<(), Box<dyn Error>> {
    let mut bytes = b"\"".to_vec();
    bytes.resize(128 * 1_024 + 1, b'a');
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn declared_json_follow_up_respects_the_validation_ceiling() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"true ".to_vec());
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 4;

    let inspection = support::inspect_with_ceilings(&source, "application/json", ceilings).await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn truncated_declared_json_scalar_preserves_the_size_reason() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"true".to_vec());
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 1;

    let inspection = support::inspect_with_ceilings(&source, "application/json", ceilings).await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn truncated_declared_negative_json_number_preserves_the_size_reason()
-> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"-1".to_vec());
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 1;

    let inspection = support::inspect_with_ceilings(&source, "application/json", ceilings).await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn truncated_declared_json_exponent_preserves_the_size_reason() -> Result<(), Box<dyn Error>>
{
    let source = MemorySource::new(b"1e2".to_vec());
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 2;

    let inspection = support::inspect_with_ceilings(&source, "application/json", ceilings).await?;
    support::assert_malformed_reason(inspection, "source_too_large");
    Ok(())
}

#[tokio::test]
async fn impossible_declared_json_fraction_prefix_uses_bounded_text_fallback()
-> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"1.e2".to_vec());
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 3;

    let inspection = support::inspect_with_ceilings(&source, "application/json", ceilings).await?;
    support::assert_declared_mismatch(
        inspection,
        DeclaredMismatchExpectation {
            declared: "application/json",
            detected: "text/plain",
        },
    );
    Ok(())
}

#[tokio::test]
async fn oversized_declared_json_with_trailing_prose_uses_bounded_text_fallback()
-> Result<(), Box<dyn Error>> {
    let mut bytes = b"true trailing".to_vec();
    bytes.resize(128 * 1_024 + 1, b' ');
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_declared_mismatch(
        inspection,
        DeclaredMismatchExpectation {
            declared: "application/json",
            detected: "text/plain",
        },
    );
    Ok(())
}

#[tokio::test]
async fn oversized_declared_json_with_a_split_scalar_uses_bounded_text_fallback()
-> Result<(), Box<dyn Error>> {
    let mut bytes = b"true ".to_vec();
    bytes.resize(4_095, b' ');
    bytes.extend_from_slice("é".as_bytes());
    bytes.resize(128 * 1_024 + 1, b' ');
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_declared_mismatch(
        inspection,
        DeclaredMismatchExpectation {
            declared: "application/json",
            detected: "text/plain",
        },
    );
    Ok(())
}

#[tokio::test]
async fn non_json_ascii_whitespace_before_an_object_uses_text_fallback()
-> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"\x0b{}".to_vec());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
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
async fn csv_probe_does_not_claim_structurally_valid_json() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_array_formatted_like_csv());

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_validated_media(inspection, "application/json");
    Ok(())
}

#[tokio::test]
async fn incomplete_json_and_complete_csv_probe_tie_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"[1,2\n,3".to_vec());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_unknown(inspection);
    Ok(())
}

#[tokio::test]
async fn stronger_truncated_json_probe_is_malformed_before_csv() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_with_json_consistent_truncated_prefix());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_malformed_reason(inspection, "malformed_json");
    Ok(())
}

#[tokio::test]
async fn csv_like_truncated_prefix_does_not_suppress_valid_json() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_with_csv_consistent_truncated_prefix());

    let inspection = support::inspect(&source, "application/json").await?;
    support::assert_validated_media(inspection, "application/json");
    Ok(())
}

#[tokio::test]
async fn overlapping_truncated_prefix_still_detects_json() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_with_csv_consistent_truncated_prefix());

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
async fn stronger_truncated_json_probe_is_malformed_before_declared_text()
-> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_with_json_consistent_truncated_prefix());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_malformed_reason(inspection, "malformed_json");
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
async fn complete_json_source_ending_mid_scalar_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::json_then_incomplete_scalar());

    let inspection = support::inspect(&source, "application/octet-stream").await?;
    support::assert_unknown(inspection);
    Ok(())
}

#[tokio::test]
async fn complete_csv_source_ending_mid_scalar_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_then_incomplete_scalar());

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
async fn complete_json_prefix_without_eof_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::complete_json_prefix_followed_outside_probe());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_unknown(inspection);
    Ok(())
}

#[tokio::test]
async fn completed_json_prefix_followed_by_whitespace_is_structurally_detected()
-> Result<(), Box<dyn Error>> {
    let mut bytes = b"{}".to_vec();
    bytes.resize(4_097, b' ');
    let source = MemorySource::new(bytes);

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
async fn completed_json_prefix_with_split_utf8_suffix_is_unknown() -> Result<(), Box<dyn Error>> {
    let mut bytes = b"{}".to_vec();
    bytes.resize(4_095, b' ');
    bytes.extend_from_slice("é prose".as_bytes());
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_unknown(inspection);
    Ok(())
}

#[tokio::test]
async fn nonprovisional_json_prefix_with_later_trailing_prose_is_malformed()
-> Result<(), Box<dyn Error>> {
    let mut bytes = b"{\"padding\":\"".to_vec();
    bytes.resize(4_097, b'a');
    bytes.extend_from_slice(b"\"} prose");
    let source = MemorySource::new(bytes);

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_malformed_reason(inspection, "malformed_json");
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
async fn declared_one_column_csv_preserves_malformed_quotes() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"header\n\"unterminated\n".to_vec());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_malformed_reason(inspection, "malformed_csv");
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
async fn declared_header_only_csv_preserves_the_column_limit_reason() -> Result<(), Box<dyn Error>>
{
    let mut header = (0..257)
        .map(|index| format!("column{index}"))
        .collect::<Vec<_>>()
        .join(",");
    header.push('\n');
    let source = MemorySource::new(header.into_bytes());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_malformed_reason(inspection, "column_limit_exceeded");
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
async fn unprobed_declared_csv_candidate_uses_text_fallback() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"hello".to_vec());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_declared_mismatch(
        inspection,
        DeclaredMismatchExpectation {
            declared: "text/csv",
            detected: "text/plain",
        },
    );
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
async fn complete_csv_probe_validates_all_records_before_claiming() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(b"a,b\nc,d\nplain prose\n".to_vec());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_validated_media(inspection, "text/plain");
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
async fn truncated_csv_probe_with_later_prose_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::csv_truncated_probe_with_trailing_prose());

    let inspection = support::inspect(&source, "text/plain").await?;
    support::assert_unknown(inspection);
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
async fn csv_row_bomb_probe_tie_is_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::row_bomb_csv());

    let inspection = support::inspect(&source, "text/csv").await?;
    support::assert_unknown(inspection);
    Ok(())
}

#[tokio::test]
async fn declared_one_column_csv_preserves_the_row_limit_reason() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(fixtures::one_column_row_bomb_csv());

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
/// A generated five-GiB source retains only the requested frame, never its declared length.
struct StreamedTextSource {
    maximum_requested: std::sync::atomic::AtomicU64,
    requested: std::sync::atomic::AtomicU64,
}

impl signalbox_file_media_runtime::VerifiedBlobSource for StreamedTextSource {
    fn digest(&self) -> signalbox_file_media_runtime::FileDigest {
        signalbox_file_media_runtime::FileDigest::from_bytes([0x51; 32])
    }
    fn byte_length(&self) -> std::num::NonZeroU64 {
        const { std::num::NonZeroU64::new(5 * 1024 * 1024 * 1024).expect("five GiB is positive") }
    }
    fn read_range(
        &self,
        offset: u64,
        length: std::num::NonZeroU64,
    ) -> signalbox_file_media_runtime::SourceReadFuture<'_> {
        Box::pin(async move {
            use std::sync::atomic::Ordering::Relaxed;
            self.maximum_requested.fetch_max(length.get(), Relaxed);
            self.requested.fetch_add(length.get(), Relaxed);
            if length.get() > 131_072
                || offset
                    .checked_add(length.get())
                    .is_none_or(|end| end > self.byte_length().get())
            {
                return Err(signalbox_file_media_runtime::SourceReadError::RangeOutOfBounds);
            }
            Ok(vec![b'x'; length.get() as usize])
        })
    }
}

#[tokio::test]
async fn five_gibibyte_text_uses_bounded_prefixes_and_continued_sections()
-> Result<(), Box<dyn Error>> {
    use signalbox_file_media_runtime::*;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    let source = StreamedTextSource {
        maximum_requested: AtomicU64::new(0),
        requested: AtomicU64::new(0),
    };
    let registry = FileMediaRegistry::try_new(
        vec![
            signalbox_file_media_adapters_text::text_family_declaration()
                .map_err(|_| "declaration")?,
        ],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )?;
    let inspection = InspectionRequest {
        source: FileUse::new(
            source.digest(),
            source.byte_length(),
            AttachmentKind::File,
            DeclaredMediaType::try_new("text/plain")?,
            None,
        ),
        visible_part: None,
    };
    let processor = DirectProcessor::provider();
    let inspected = registry
        .inspect(&processor, inspection.clone(), &source, &NeverCancelled)
        .await?;
    support::assert_validated_media(inspected, "text/plain");
    assert_eq!(source.maximum_requested.load(Relaxed), 4096);
    assert_eq!(source.requested.load(Relaxed), 12_288);

    let first = registry
        .read(
            &processor,
            FileReadRequest {
                inspection: inspection.clone(),
                view: ReadViewName::try_new("text")?,
                input: FileReadInput::Initial {
                    options: serde_json::json!({}),
                },
            },
            &source,
            &NeverCancelled,
        )
        .await?;
    let FileReadResult::Text {
        body,
        continuation: ReadContinuation::More { cursor },
    } = first
    else {
        panic!("the first bounded section must have a continuation")
    };
    assert_eq!(body.len(), 131_069);
    assert_eq!(cursor.as_str(), "section_1");
    drop(body);
    let second = registry
        .read(
            &processor,
            FileReadRequest {
                inspection,
                view: ReadViewName::try_new("text")?,
                input: FileReadInput::Continuation { cursor },
            },
            &source,
            &NeverCancelled,
        )
        .await?;
    let FileReadResult::Text {
        body,
        continuation: ReadContinuation::More { cursor },
    } = second
    else {
        panic!("the next bounded section must have a continuation")
    };
    assert_eq!(body.len(), 131_069);
    assert_eq!(cursor.as_str(), "section_2");
    assert_eq!(source.maximum_requested.load(Relaxed), 131_072);
    assert_eq!(source.requested.load(Relaxed), 299_008);
    Ok(())
}

#[tokio::test]
async fn text_sections_preserve_a_scalar_crossing_the_boundary() -> Result<(), Box<dyn Error>> {
    use signalbox_file_media_runtime::*;
    let mut text = "a".repeat(131_068);
    text.push_str("🦀tail");
    let source = MemorySource::new(text.clone().into_bytes());
    let registry = FileMediaRegistry::try_new(
        vec![
            signalbox_file_media_adapters_text::text_family_declaration()
                .map_err(|_| "declaration")?,
        ],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )?;
    let inspection = InspectionRequest {
        source: source.file_use("text/plain")?,
        visible_part: None,
    };
    let processor = DirectProcessor::provider();
    let first = registry
        .read(
            &processor,
            FileReadRequest {
                inspection: inspection.clone(),
                view: ReadViewName::try_new("text")?,
                input: FileReadInput::Initial {
                    options: serde_json::json!({}),
                },
            },
            &source,
            &NeverCancelled,
        )
        .await?;
    let FileReadResult::Text {
        body: first,
        continuation: ReadContinuation::More { cursor },
    } = first
    else {
        panic!("the boundary leaves a tail")
    };
    assert!(first.ends_with('🦀'));
    let second = registry
        .read(
            &processor,
            FileReadRequest {
                inspection,
                view: ReadViewName::try_new("text")?,
                input: FileReadInput::Continuation { cursor },
            },
            &source,
            &NeverCancelled,
        )
        .await?;
    let FileReadResult::Text {
        body: second,
        continuation: ReadContinuation::Complete,
    } = second
    else {
        panic!("the next section completes the source")
    };
    assert_eq!(second, "tail");
    assert_eq!(first + &second, text);
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the delegated real file-media sandbox profile"]
async fn file_read_five_gibibyte_source_stays_within_worker_memory_limit()
-> Result<(), Box<dyn Error>> {
    use signalbox_file_media_processor_runtime::{SandboxedFileMediaProcessor, WorkerBinding};
    use signalbox_file_media_runtime::*;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    let source = StreamedTextSource {
        maximum_requested: AtomicU64::new(0),
        requested: AtomicU64::new(0),
    };
    let declaration =
        signalbox_file_media_adapters_text::text_family_declaration().map_err(|_| "declaration")?;
    let worker = signalbox_test_bin::test_bin_path!("signalbox-file-media-text-worker");
    let limits = FileMediaProcessCeilings::version_one();
    assert!(limits.memory_bytes() < source.byte_length().get());
    let processor = SandboxedFileMediaProcessor::try_new(
        "/usr/bin/bwrap",
        vec![WorkerBinding::try_new(worker, declaration.clone())?],
        limits,
    )?;
    assert_eq!(
        processor.verify_isolation().await,
        ProcessorIsolation::Available
    );
    let registry = FileMediaRegistry::try_new(
        vec![declaration],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )?;
    let inspection = InspectionRequest {
        source: FileUse::new(
            source.digest(),
            source.byte_length(),
            AttachmentKind::File,
            DeclaredMediaType::try_new("text/plain")?,
            None,
        ),
        visible_part: None,
    };
    let first = registry
        .read(
            &processor,
            FileReadRequest {
                inspection: inspection.clone(),
                view: ReadViewName::try_new("text")?,
                input: FileReadInput::Initial {
                    options: serde_json::json!({}),
                },
            },
            &source,
            &NeverCancelled,
        )
        .await?;
    let FileReadResult::Text {
        body,
        continuation: ReadContinuation::More { cursor },
    } = first
    else {
        panic!("the sandboxed first section must continue")
    };
    assert_eq!(body.len(), 131_069);
    drop(body);
    let second = registry
        .read(
            &processor,
            FileReadRequest {
                inspection,
                view: ReadViewName::try_new("text")?,
                input: FileReadInput::Continuation { cursor },
            },
            &source,
            &NeverCancelled,
        )
        .await?;
    let FileReadResult::Text {
        body,
        continuation: ReadContinuation::More { cursor },
    } = second
    else {
        panic!("the sandboxed second section must continue")
    };
    assert_eq!(body.len(), 131_069);
    assert_eq!(cursor.as_str(), "section_2");
    assert_eq!(source.maximum_requested.load(Relaxed), 131_072);
    assert_eq!(source.requested.load(Relaxed), 286_720);
    Ok(())
}
