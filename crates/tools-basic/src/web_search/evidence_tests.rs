use signalbox_model_runtime::CredentialValue;

use super::{diagnostic::*, evidence::*, redaction::*, result::*, test_support::*};

/// JSON expansion cannot carry completed evidence across the shared
/// `ToolResultText` bound.
#[test]
fn web_search_rejects_encoded_output_over_tool_result_bound() {
    let content = content_over_tool_result_bound();

    assert_eq!(
        completed_text_evidence(content),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: fixed JSON member names cannot collide with the credential in
/// completed evidence, even when provider fields contain no credential.
#[test]
fn web_search_final_success_payload_rejects_credential_collision() {
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        SUCCESS_PAYLOAD_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response_with_result_count(1), &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: JSON-aware error sanitization decodes an escaped credential
/// before the body can enter durable failure evidence.
#[test]
fn web_search_error_body_redacts_json_escaped_credential() {
    let escaped = ascii_json_unicode_escape(SYNTHETIC_KEY);
    let body = format!(r#"{{"error":{{"detail":"{escaped}"}}}}"#).into_bytes();
    let error = WebSearchProviderError::new(PROVIDER_REJECTION_STATUS, body)
        .expect("fixture error body is bounded");
    let detail = provider_error_detail(error, &scrubber())
        .expect("detail is admitted")
        .expect("detail does not collide");

    assert!(!detail.as_str().contains(SYNTHETIC_KEY));
}

/// INV-035: only Brave's known nested error-detail component can become
/// entity-escaped provider text in failure evidence.
#[test]
fn web_search_error_body_reads_nested_typed_detail() {
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {"detail": FIXTURE_PROVIDER_ERROR_DETAIL},
        "unknown": SYNTHETIC_KEY,
    }))
    .expect("fixture provider error encodes");
    let error = WebSearchProviderError::new(PROVIDER_REJECTION_STATUS, body)
        .expect("fixture error body is bounded");
    let detail = provider_error_detail(error, &scrubber())
        .expect("detail is admitted")
        .expect("detail does not collide");

    assert!(
        detail
            .as_str()
            .contains(FIXTURE_ESCAPED_PROVIDER_ERROR_DETAIL)
    );
    assert!(!detail.as_str().contains(SYNTHETIC_KEY));
}

/// INV-035: an unparsed provider error body contributes no text to failure
/// evidence, independently of whether it collides with the credential.
#[test]
fn web_search_unparsed_error_body_never_enters_evidence() {
    let error = WebSearchProviderError::new(
        PROVIDER_REJECTION_STATUS,
        FIXTURE_UNPARSED_PROVIDER_ERROR.as_bytes().to_vec(),
    )
    .expect("fixture error body is bounded");
    let detail = provider_error_detail(error, &scrubber())
        .expect("fixed detail is admitted")
        .expect("fixed detail does not collide");

    assert!(!detail.as_str().contains(FIXTURE_UNPARSED_PROVIDER_ERROR));
}

/// INV-035: error redaction precedes evidence truncation, so a credential
/// crossing the retained prefix is replaced before the suffix is added.
#[test]
fn web_search_error_body_is_redacted_before_truncation() {
    let reflected = format!(
        "{}{}{}",
        "a".repeat(MAX_PROVIDER_RESPONSE_BYTES / 4),
        SYNTHETIC_KEY,
        "z".repeat(MAX_PROVIDER_RESPONSE_BYTES / 4)
    );
    let body = serde_json::to_vec(&serde_json::json!({"error": {"detail": reflected}}))
        .expect("fixture provider error encodes");
    let error = WebSearchProviderError::new(PROVIDER_REJECTION_STATUS, body)
        .expect("fixture error body is bounded");
    let detail = provider_error_detail(error, &scrubber())
        .expect("detail is admitted")
        .expect("detail does not collide");

    assert!(!detail.as_str().contains(SYNTHETIC_KEY));
    assert!(detail.as_str().ends_with(TRUNCATION_SUFFIX));
}

/// INV-035: fixed provider-error prose cannot collide with the credential
/// after the provider body has been sanitized.
#[test]
fn web_search_final_error_detail_rejects_credential_collision() {
    const ERROR_PREFIX_COLLISION_KEY: &str = "rejected";
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        ERROR_PREFIX_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");
    let error = WebSearchProviderError::new(
        PROVIDER_REJECTION_STATUS,
        br#"{"error":{"detail":"synthetic rejection"}}"#.to_vec(),
    )
    .expect("fixture error body is bounded");

    assert_eq!(provider_error_detail(error, &scrubber), Ok(None));
}
