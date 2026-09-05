use reqwest::StatusCode;
use signalbox_application::ToolExecutorEvidence;
use signalbox_domain::ToolExecutionErrorDetail;
use signalbox_model_runtime::CredentialValue;

use super::{
    diagnostic::*, evidence::*, redaction::*, result::*, test_support::*, text_decoding::*,
};

/// provider-controlled successful fields are credential-scrubbed
/// before entering completed tool evidence.
#[test]
fn web_search_success_evidence_redacts_reflected_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: format!("reflected {SYNTHETIC_KEY}"),
        url: format!("{FIXTURE_RESULT_URL}?token={SYNTHETIC_KEY}"),
        snippet: format!("snippet {SYNTHETIC_KEY}"),
    })
    .expect("reflected fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let evidence = success_evidence(response, &scrubber()).expect("response encodes");
    let content = completed_text(evidence);

    assert!(!content.contains(SYNTHETIC_KEY));
}

/// the URL-specific credential gate detects an exact credential in
/// an admitted result path before completed evidence is constructed.
#[test]
fn web_search_rejects_plain_credential_in_result_url_path() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: fixture_result_url_with_path_segment(SYNTHETIC_KEY),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("credential-bearing path fixture is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");

    assert_eq!(
        success_evidence(response, &scrubber()),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// credentials colliding with fixed provider-result diagnostics are
/// rejected before any provider response or error can be formatted.
#[test]
fn web_search_rejects_credentials_colliding_with_fixed_result_diagnostics() {
    let provider_error_collision = CredentialScrubber::try_new(&CredentialValue::new(
        PROVIDER_ERROR_DEBUG_COLLISION_KEY.as_bytes().to_vec(),
    ));
    let placeholder_collision = CredentialScrubber::try_new(&CredentialValue::new(
        PROVIDER_PLACEHOLDER_DEBUG_COLLISION_KEY.as_bytes().to_vec(),
    ));

    assert!(provider_error_collision.is_none());
    assert!(placeholder_collision.is_none());
}

/// a credential spanning the populated response `Option` wrapper and
/// its result type is rejected before a constructor result can be rendered.
#[test]
fn web_search_rejects_credential_colliding_with_response_option() {
    let collision = CredentialScrubber::try_new(&CredentialValue::new(
        POPULATED_RESPONSE_OPTION_DEBUG_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let rendered = format!(
        "{:?}",
        WebSearchResponse::new(Vec::new(), WebSearchPageCompleteness::Complete)
    );

    assert!(collision.is_none());
    assert!(rendered.contains(POPULATED_RESPONSE_OPTION_DEBUG_COLLISION_KEY));
}

/// credentials spanning the populated partial-response `Option`
/// wrapper are rejected before a constructor result can be rendered.
#[test]
fn web_search_rejects_credential_colliding_with_partial_response_option() {
    let collision = CredentialScrubber::try_new(&CredentialValue::new(
        POPULATED_PARTIAL_RESPONSE_OPTION_DEBUG_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let rendered = format!(
        "{:?}",
        WebSearchResponse::new(Vec::new(), WebSearchPageCompleteness::MoreAvailable)
    );

    assert!(collision.is_none());
    assert!(rendered.contains(POPULATED_PARTIAL_RESPONSE_OPTION_DEBUG_COLLISION_KEY));
}

/// credentials spanning the populated provider-error `Option`
/// wrapper and its opaque diagnostic are rejected before public formatting.
#[test]
fn web_search_rejects_credential_colliding_with_provider_error_option() {
    let collision = CredentialScrubber::try_new(&CredentialValue::new(
        POPULATED_PROVIDER_ERROR_OPTION_DEBUG_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let rendered = format!(
        "{:?}",
        WebSearchProviderError::new(StatusCode::BAD_REQUEST.as_u16(), Vec::new())
    );

    assert!(collision.is_none());
    assert!(rendered.contains(POPULATED_PROVIDER_ERROR_OPTION_DEBUG_COLLISION_KEY));
}

/// credentials colliding with fixed provider, policy, or
/// configuration Debug labels are rejected before egress objects can render.
#[test]
fn web_search_rejects_credentials_colliding_with_fixed_egress_diagnostics() {
    let provider_collision = CredentialScrubber::try_new(&CredentialValue::new(
        PROVIDER_VARIANT_DEBUG_COLLISION_KEY.as_bytes().to_vec(),
    ));
    let policy_collision = CredentialScrubber::try_new(&CredentialValue::new(
        EGRESS_POLICY_DEBUG_COLLISION_KEY.as_bytes().to_vec(),
    ));
    let configuration_collision = CredentialScrubber::try_new(&CredentialValue::new(
        CONFIGURATION_DEBUG_COLLISION_KEY.as_bytes().to_vec(),
    ));
    let configuration = configuration();

    assert!(provider_collision.is_none());
    assert!(policy_collision.is_none());
    assert!(configuration_collision.is_none());
    assert!(
        format!("{:?}", configuration.provider()).contains(PROVIDER_VARIANT_DEBUG_COLLISION_KEY)
    );
    assert!(
        format!("{:?}", configuration.egress_policy()).contains(EGRESS_POLICY_DEBUG_COLLISION_KEY)
    );
    assert!(format!("{configuration:?}").contains(CONFIGURATION_DEBUG_COLLISION_KEY));
}

/// credentials colliding with fixed executor-evidence Debug labels
/// are rejected before any terminal evidence can be formatted.
#[test]
fn web_search_rejects_credentials_colliding_with_evidence_debug_labels() {
    let completed_collision = CredentialScrubber::try_new(&CredentialValue::new(
        COMPLETED_EVIDENCE_DEBUG_COLLISION_KEY.as_bytes().to_vec(),
    ));
    let known_failure_collision = CredentialScrubber::try_new(&CredentialValue::new(
        KNOWN_FAILURE_EVIDENCE_DEBUG_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let success_result_collision = CredentialScrubber::try_new(&CredentialValue::new(
        SUCCESS_RESULT_DEBUG_COLLISION_KEY.as_bytes().to_vec(),
    ));
    let success_result_boundary_collision = CredentialScrubber::try_new(&CredentialValue::new(
        SUCCESS_RESULT_BOUNDARY_DEBUG_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let populated_option_collision = CredentialScrubber::try_new(&CredentialValue::new(
        POPULATED_EVIDENCE_OPTION_DEBUG_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let populated_detail_collision = CredentialScrubber::try_new(&CredentialValue::new(
        POPULATED_EVIDENCE_DETAIL_DEBUG_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let populated_success_escape_collision = CredentialScrubber::try_new(&CredentialValue::new(
        POPULATED_SUCCESS_DEBUG_ESCAPE_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let detail = ToolExecutionErrorDetail::try_new(String::from(FIXTURE_PROVIDER_ERROR_DETAIL))
        .expect("fixture error detail is admitted");
    let populated = format!(
        "{:?}",
        ToolExecutorEvidence::KnownFailed {
            detail: Some(detail)
        }
    );
    let empty_response = WebSearchResponse::new(Vec::new(), WebSearchPageCompleteness::Complete)
        .expect("empty fixture response is admitted");
    let empty_page_evidence =
        success_evidence(empty_response, &scrubber()).expect("empty response encodes");
    let rendered_empty_page = format!("{empty_page_evidence:?}");
    let result_response = WebSearchResponse::new(Vec::new(), WebSearchPageCompleteness::Complete)
        .expect("empty result-wrapper fixture response is admitted");
    let rendered_result = format!("{:?}", success_evidence(result_response, &scrubber()));

    assert!(completed_collision.is_none());
    assert!(known_failure_collision.is_none());
    assert!(success_result_collision.is_none());
    assert!(success_result_boundary_collision.is_none());
    assert!(populated_option_collision.is_none());
    assert!(populated_detail_collision.is_none());
    assert!(populated_success_escape_collision.is_none());
    assert!(populated.contains(POPULATED_EVIDENCE_OPTION_DEBUG_COLLISION_KEY));
    assert!(populated.contains(POPULATED_EVIDENCE_DETAIL_DEBUG_COLLISION_KEY));
    assert!(rendered_empty_page.contains(POPULATED_SUCCESS_DEBUG_ESCAPE_COLLISION_KEY));
    assert!(rendered_result.contains(SUCCESS_RESULT_DEBUG_COLLISION_KEY));
    assert!(rendered_result.contains(SUCCESS_RESULT_BOUNDARY_DEBUG_COLLISION_KEY));
}

/// a credential spanning the populated error `Result` wrapper and
/// its error variant is rejected before any error evidence can be rendered.
#[test]
fn web_search_rejects_credential_colliding_with_populated_error_result() {
    let collision = CredentialScrubber::try_new(&CredentialValue::new(
        ERROR_RESULT_BOUNDARY_DEBUG_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let rendered = format!(
        "{:?}",
        Err::<ToolExecutorEvidence, WebSearchExecutorError>(
            WebSearchExecutorError::EvidenceEncoding
        )
    );

    assert!(collision.is_none());
    assert!(rendered.contains(ERROR_RESULT_BOUNDARY_DEBUG_COLLISION_KEY));
}

/// a credential spanning the successful `Result` wrapper and
/// `KnownFailed` evidence is rejected before any failure can be rendered.
#[test]
fn web_search_rejects_credential_colliding_with_known_failure_result() {
    let collision = CredentialScrubber::try_new(&CredentialValue::new(
        KNOWN_FAILURE_RESULT_BOUNDARY_DEBUG_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let rendered = format!(
        "{:?}",
        Ok::<ToolExecutorEvidence, WebSearchExecutorError>(ToolExecutorEvidence::KnownFailed {
            detail: None
        })
    );

    assert!(collision.is_none());
    assert!(rendered.contains(KNOWN_FAILURE_RESULT_BOUNDARY_DEBUG_COLLISION_KEY));
}

/// a credential spanning the successful `Result` wrapper and a
/// populated `KnownFailed` detail is rejected before it can be rendered.
#[test]
fn web_search_rejects_credential_colliding_with_populated_known_failure_result() {
    let collision = CredentialScrubber::try_new(&CredentialValue::new(
        POPULATED_FAILURE_RESULT_DEBUG_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ));
    let detail = ToolExecutionErrorDetail::try_new(String::from(FIXTURE_PROVIDER_ERROR_DETAIL))
        .expect("fixture error detail is admitted");
    let rendered = format!(
        "{:?}",
        Ok::<ToolExecutorEvidence, WebSearchExecutorError>(ToolExecutorEvidence::KnownFailed {
            detail: Some(detail)
        })
    );

    assert!(collision.is_none());
    assert!(rendered.contains(POPULATED_FAILURE_RESULT_DEBUG_COLLISION_KEY));
}

/// Legacy percent-encoded URL bytes that are not UTF-8 remain usable because
/// comparison-only decoding is lossy while credential checks stay fail-closed.
#[test]
fn web_search_retains_legacy_non_utf8_percent_encoded_result_url() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_LEGACY_PERCENT_ENCODED_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("legacy percent-encoded fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");

    let evidence = success_evidence(response, &scrubber()).expect("response encodes");
    let content = completed_text(evidence);

    assert!(content.contains(FIXTURE_LEGACY_PERCENT_ENCODED_RESULT_URL));
}

/// Removing the arbitrary detail probe payload from the fixed Debug inventory
/// does not reject otherwise usable credentials that match its words.
#[test]
fn web_search_accepts_credentials_matching_removed_diagnostic_probe_text() {
    let diagnostic = CredentialScrubber::try_new(&CredentialValue::new(
        REMOVED_DIAGNOSTIC_PROBE_WORD.as_bytes().to_vec(),
    ));
    let probe = CredentialScrubber::try_new(&CredentialValue::new(
        REMOVED_DETAIL_PROBE_WORD.as_bytes().to_vec(),
    ));

    assert!(diagnostic.is_some());
    assert!(probe.is_some());
}

/// JSON Unicode escapes in provider text are decoded within the
/// bounded scrubber before completed evidence is formed.
#[test]
fn web_search_success_evidence_redacts_json_unicode_escaped_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: format!("Synthetic {JSON_UNICODE_COLLISION_VALUE} result"),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("JSON-escaped fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        JSON_UNICODE_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
    let content = completed_text(evidence);

    assert!(!content.contains(JSON_UNICODE_COLLISION_VALUE));
    assert!(!unicode_case_insensitive_contains(
        &content,
        JSON_UNICODE_COLLISION_KEY,
    ));
}

/// reversible short JSON escapes in the credential itself apply
/// before provider-controlled fields enter completed evidence.
#[test]
fn web_search_success_evidence_redacts_json_solidus_decoded_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(JSON_SOLIDUS_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("JSON solidus fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        JSON_SOLIDUS_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
    let content = completed_text(evidence);

    assert!(!content.contains(JSON_SOLIDUS_COLLISION_KEY));
    assert!(!content.contains(JSON_SOLIDUS_COLLISION_VALUE));
}

/// a brace-delimited Rust Debug Unicode escape in the credential
/// is decoded before provider text or completed-evidence Debug can reflect it.
#[test]
fn web_search_success_evidence_redacts_rust_debug_unicode_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(RUST_DEBUG_UNICODE_COLLISION_VALUE),
    })
    .expect("Rust Debug Unicode fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        RUST_DEBUG_UNICODE_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
    let rendered = format!("{evidence:?}");
    let content = completed_text(evidence);

    assert!(!rendered.contains(RUST_DEBUG_UNICODE_COLLISION_KEY));
    assert!(!content.contains(RUST_DEBUG_UNICODE_COLLISION_KEY));
    assert!(!content.contains(RUST_DEBUG_UNICODE_COLLISION_VALUE));
}

/// multi-character full Unicode folding applies to provider text
/// before completed evidence is formed.
#[test]
fn web_search_success_evidence_redacts_full_case_folded_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: format!("Synthetic {UNICODE_FULL_FOLD_COLLISION_VALUE} result"),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("full-fold fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        UNICODE_FULL_FOLD_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
    let content = completed_text(evidence);

    assert!(!unicode_case_insensitive_contains(
        &content,
        UNICODE_FULL_FOLD_COLLISION_KEY,
    ));
}

/// reversible decoding of the credential itself applies before
/// provider-controlled fields enter completed evidence.
#[test]
fn web_search_success_evidence_redacts_text_matching_decoded_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(REVERSE_ENCODED_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("decoded credential fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        REVERSE_ENCODED_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
    let content = completed_text(evidence);

    assert!(!content.contains(REVERSE_ENCODED_COLLISION_KEY));
    assert!(!content.contains(REVERSE_ENCODED_COLLISION_VALUE));
}

/// a terminated standard named HTML reference is decoded before
/// reversible output can expose a credential.
#[test]
fn web_search_redacts_terminated_standard_named_reference_in_result_text() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(UNSUPPORTED_NAMED_ENTITY_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("unsupported named-entity fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        UNSUPPORTED_NAMED_ENTITY_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
    let content = completed_text(evidence);

    assert!(!content.contains(UNSUPPORTED_NAMED_ENTITY_COLLISION_KEY));
    assert!(!content.contains(UNSUPPORTED_NAMED_ENTITY_COLLISION_VALUE));
    assert!(!content.contains(UNSUPPORTED_NAMED_ENTITY_ESCAPED_VALUE));
}

#[test]
fn web_search_decodes_supported_named_nonbreaking_space_reference() {
    let source = format!("&{SUPPORTED_NAMED_NONBREAKING_SPACE_REFERENCE};");
    let decoded = decode_html_character_references(&source);

    assert_eq!(decoded.text, SUPPORTED_NAMED_NONBREAKING_SPACE_VALUE);
    assert_eq!(decoded.change, ReversibleTextChange::Changed);
}

/// Common named references in provider prose remain available as safely
/// entity-escaped result text.
#[test]
fn web_search_preserves_common_named_references_as_entity_escaped_text() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_COMMON_NAMED_REFERENCES_SNIPPET),
    })
    .expect("common named-reference fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");

    let evidence = success_evidence(response, &scrubber()).expect("response encodes safely");
    let content = completed_text(evidence);

    assert!(content.contains(FIXTURE_ESCAPED_COMMON_NAMED_REFERENCES_SNIPPET));
}

/// signed digits are not valid JSON Unicode or HTML numeric
/// references and cannot be normalized into provider evidence.
#[test]
fn web_search_rejects_signed_numeric_escape_digits() {
    let json = decode_json_string_escapes(SIGNED_JSON_UNICODE_ESCAPE);
    let html_source = format!("&{SIGNED_HTML_NUMERIC_REFERENCE};");
    let html = decode_html_character_references(&html_source);

    assert_eq!(json.text, SIGNED_JSON_UNICODE_ESCAPE);
    assert_eq!(json.change, ReversibleTextChange::Unchanged);
    assert_eq!(html.text, html_source);
    assert_eq!(html.change, ReversibleTextChange::Unchanged);
}

/// The HTML library preserves an unknown terminated name as provider text,
/// which typed evidence escapes again.
#[test]
fn web_search_preserves_entity_escaped_unknown_named_references() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_UNKNOWN_NAMED_REFERENCE_TITLE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_UNSUPPORTED_VALID_NAMED_REFERENCE_SNIPPET),
    })
    .expect("unknown named-reference fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");

    let evidence = success_evidence(response, &scrubber()).expect("response encodes safely");
    let content = completed_text(evidence);

    let escaped =
        html_escape::encode_quoted_attribute(FIXTURE_UNSUPPORTED_VALID_NAMED_REFERENCE_SNIPPET);
    assert!(content.contains(escaped.as_ref()));
}

/// credential removal cannot turn an entity-escaped literal into
/// markup-bearing output after typed result construction.
#[test]
fn web_search_rejects_credential_collision_with_entity_escape_syntax() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_LITERAL_ENTITY_TITLE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("literal entity fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        ENTITY_ESCAPE_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// Semicolonless numeric syntax is ordinary provider text to the library
/// decoder and remains entity-escaped in typed evidence.
#[test]
fn web_search_preserves_semicolonless_numeric_reference_in_result_text() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(SEMICOLONLESS_NUMERIC_HTML_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("semicolonless numeric-reference fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        SEMICOLONLESS_NUMERIC_HTML_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
    let content = completed_text(evidence);

    let escaped = html_escape::encode_quoted_attribute(SEMICOLONLESS_NUMERIC_HTML_COLLISION_VALUE);
    assert!(content.contains(escaped.as_ref()));
}

/// Semicolonless named syntax is ordinary provider text to the library
/// decoder and remains entity-escaped in typed evidence.
#[test]
fn web_search_preserves_semicolonless_named_reference_in_result_text() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(SEMICOLONLESS_NAMED_HTML_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("semicolonless named-reference fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        SEMICOLONLESS_NAMED_HTML_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
    let content = completed_text(evidence);

    let escaped = html_escape::encode_quoted_attribute(SEMICOLONLESS_NAMED_HTML_COLLISION_VALUE);
    assert!(content.contains(escaped.as_ref()));
}

/// an unknown named-reference prefix cannot hide a later recognized
/// reference from credential decoding at a nested ampersand.
#[test]
fn web_search_redacts_recognized_reference_after_nested_ampersand() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(NESTED_NAMED_HTML_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("nested named-reference fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        SEMICOLONLESS_NAMED_HTML_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
    let content = completed_text(evidence);

    assert!(!content.contains(SEMICOLONLESS_NAMED_HTML_COLLISION_KEY));
    assert!(!content.contains(NESTED_NAMED_HTML_COLLISION_VALUE));
}

/// A prefixed legacy name is ordinary provider text to the library decoder
/// and remains entity-escaped in typed evidence.
#[test]
fn web_search_preserves_prefixed_legacy_named_reference_in_result_text() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(PREFIXED_LEGACY_NAMED_HTML_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("prefixed legacy named-reference fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        PREFIXED_LEGACY_NAMED_HTML_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
    let content = completed_text(evidence);

    let escaped = html_escape::encode_quoted_attribute(PREFIXED_LEGACY_NAMED_HTML_COLLISION_VALUE);
    assert!(content.contains(escaped.as_ref()));
}

/// credential scrubbing cannot turn a checked result title into
/// an empty title in completed evidence.
#[test]
fn web_search_rejects_result_with_title_invalidated_by_credential_scrubbing() {
    const TITLE_COLLISION_KEY: &str = "synthetic-title-key";
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(TITLE_COLLISION_KEY),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("reflected fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        TITLE_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// credential scrubbing cannot turn a checked result URL into an
/// invalid URL in completed evidence.
#[test]
fn web_search_rejects_result_with_url_invalidated_by_credential_scrubbing() {
    let response = WebSearchResponse::new(
        vec![result(FIXTURE_RESULT_TITLE)],
        WebSearchPageCompleteness::Complete,
    )
    .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_SCHEME_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// a standard HTML character reference cannot conceal a
/// credential reflected in provider-controlled text.
#[test]
fn web_search_redacts_html_encoded_credential_in_result_text() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(HTML_ENTITY_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("HTML entity fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        HTML_ENTITY_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
    let content = completed_text(evidence);

    assert!(!content.contains(HTML_ENTITY_COLLISION_KEY));
    assert!(!content.contains(HTML_ENTITY_COLLISION_VALUE));
}

/// The HTML library decodes a numeric reference independently of the source
/// spelling's leading-zero width before defense-in-depth scrubbing.
#[test]
fn web_search_preserves_over_window_numeric_reference_in_result_text() {
    let reflection = over_window_numeric_html_reflection();
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: reflection.clone(),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("over-window numeric-reference fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        OVER_WINDOW_NUMERIC_HTML_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
    let content = completed_text(evidence);

    assert!(!content.contains(OVER_WINDOW_NUMERIC_HTML_COLLISION_KEY));
    assert!(!content.contains(&reflection));
}

/// an HTML C1 numeric reference is decoded through its standard
/// replacement mapping before provider evidence is retained.
#[test]
fn web_search_redacts_c1_numeric_reference_credential_in_result_text() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(HTML_NUMERIC_C1_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("HTML numeric-reference fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        HTML_NUMERIC_C1_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
    let content = completed_text(evidence);

    assert!(!content.contains(HTML_NUMERIC_C1_COLLISION_KEY));
    assert!(!content.contains(HTML_NUMERIC_C1_COLLISION_VALUE));
}

/// canonical Unicode normalization cannot conceal a credential
/// reflected in an ordinary provider title.
#[test]
fn web_search_redacts_unicode_normalized_credential_in_result_text() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(URL_UNICODE_HOST_COLLISION_KEY),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("Unicode text fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
    let content = completed_text(evidence);

    assert!(!content.contains(URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY));
    assert!(!content.contains(URL_UNICODE_HOST_COLLISION_KEY));
}

/// decomposition-preserving normalization detects a credential
/// substring whose first scalar is a combining mark.
#[test]
fn web_search_redacts_unicode_combining_mark_boundary_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(UNICODE_COMBINING_MARK_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("combining-mark fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        UNICODE_COMBINING_MARK_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
    let content = completed_text(evidence);

    assert!(!content.contains(UNICODE_COMBINING_MARK_COLLISION_KEY));
    assert!(!content.contains(UNICODE_COMBINING_MARK_COLLISION_VALUE));
}

/// repeated HTML character-reference decoding cannot conceal a
/// credential reflected in provider-controlled text.
#[test]
fn web_search_redacts_nested_html_encoded_credential_in_result_text() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(HTML_NESTED_ENTITY_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("nested HTML entity fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        HTML_ENTITY_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
    let content = completed_text(evidence);

    assert!(!content.contains(HTML_ENTITY_COLLISION_KEY));
    assert!(!content.contains(HTML_NESTED_ENTITY_COLLISION_VALUE));
}

/// composed form and HTML decoding cannot conceal a credential
/// reflected in provider-controlled text.
#[test]
fn web_search_redacts_form_then_html_encoded_credential_in_result_text() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FORM_HTML_COLLISION_VALUE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("cross-codec fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        HTML_ENTITY_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
    let content = completed_text(evidence);

    assert!(!content.contains(HTML_ENTITY_COLLISION_KEY));
    assert!(!content.contains(FORM_HTML_COLLISION_VALUE));
}

/// an early HTML reference is still decoded when a later
/// multibyte scalar crosses the character-reference scan bound.
#[test]
fn web_search_html_reference_scan_handles_multibyte_boundaries() {
    let reflection = html_multibyte_boundary_reflection();

    assert!(encoded_contains_credential(
        &reflection,
        HTML_ENTITY_COLLISION_KEY
    ));
}

/// The library decoder leaves a non-reference with a distant terminator
/// unchanged; the provider-body limit bounds the surrounding input.
#[test]
fn web_search_html_reference_decoder_preserves_distant_terminator() {
    let source = distant_html_reference_terminator();
    let decoded = decode_html_character_references(&source);

    assert_eq!(decoded.text, source);
    assert_eq!(decoded.change, ReversibleTextChange::Unchanged);
}

/// credential removal cannot reproduce a key that overlaps the
/// ordinary redaction sentinel.
#[test]
fn web_search_redaction_sentinel_cannot_reproduce_credential() {
    const SENTINEL_OVERLAPPING_KEY: &str = "acted";
    const SHAPED_SECRET: &str = "SYNTHETIC-SHAPED-SECRET";
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: format!("x{SENTINEL_OVERLAPPING_KEY}x"),
        url: format!("{FIXTURE_RESULT_URL}?q={SENTINEL_OVERLAPPING_KEY}"),
        snippet: format!("y{SENTINEL_OVERLAPPING_KEY}y api_key={SHAPED_SECRET}"),
    })
    .expect("reflected fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        SENTINEL_OVERLAPPING_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");
    let evidence = success_evidence(response, &scrubber).expect("response encodes");
    let content = completed_text(evidence);

    assert!(!content.contains(SENTINEL_OVERLAPPING_KEY));
    assert!(!content.contains(SHAPED_SECRET));
}
