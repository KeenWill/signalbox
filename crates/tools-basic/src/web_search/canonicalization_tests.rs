use signalbox_model_runtime::CredentialValue;

use super::{diagnostic::*, evidence::*, redaction::*, result::*, test_support::*};

/// INV-035: an unapproved query parameter is structurally absent from output,
/// even when its value is a reversibly percent-encoded credential.
#[test]
fn web_search_drops_percent_encoded_credential_in_unapproved_query() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: format!("{FIXTURE_RESULT_URL}?token={URL_ENCODED_COLLISION_VALUE}"),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("encoded fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_ENCODED_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("typed result encodes");
    let content = completed_text(evidence);

    assert!(!content.contains(URL_ENCODED_COLLISION_VALUE));
}

/// INV-035: an unapproved query parameter is structurally absent from output,
/// even when its value is a form-encoded credential.
#[test]
fn web_search_drops_form_encoded_credential_in_unapproved_query() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: format!("{FIXTURE_RESULT_URL}?token={URL_FORM_COLLISION_VALUE}"),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("form-encoded fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_FORM_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    let evidence = success_evidence(response, &scrubber).expect("typed result encodes");
    let content = completed_text(evidence);

    assert!(!content.contains(URL_FORM_COLLISION_VALUE));
}

/// INV-035: IDNA serialization cannot retain a reversible credential in a
/// provider result host.
#[test]
fn web_search_rejects_idna_encoded_credential_in_result_host() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(URL_IDNA_COLLISION_VALUE),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("IDNA fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IDNA_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: case-insensitive domain canonicalization cannot conceal a
/// credential reflected in a provider result host.
#[test]
fn web_search_rejects_case_normalized_credential_in_result_host() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_ORIGIN_ONLY_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("domain fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_HOST_CASE_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: canonical Unicode normalization cannot conceal a decomposed
/// credential reflected in a provider result host.
#[test]
fn web_search_rejects_unicode_normalized_credential_in_result_host() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_UNICODE_EMBEDDED_HOST_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("Unicode host fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: case-insensitive scheme canonicalization cannot conceal a
/// credential reflected in a provider result URL.
#[test]
fn web_search_rejects_case_normalized_credential_in_result_scheme() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_UPPERCASE_SCHEME_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("scheme fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_SCHEME_CASE_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: URL backslash normalization cannot conceal a credential in a
/// provider result path.
#[test]
fn web_search_rejects_backslash_normalized_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_BACKSLASH_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("backslash fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_BACKSLASH_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: URL preprocessing cannot conceal a credential containing an
/// internal ASCII tab in completed provider result evidence.
#[test]
fn web_search_rejects_tab_preprocessed_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_TAB_NORMALIZED_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("tab-normalized fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_INTERNAL_TAB_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: URL preprocessing of a reversibly decoded leading C0 control
/// cannot conceal a credential in completed provider result evidence.
#[test]
fn web_search_rejects_c0_preprocessed_decoded_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_C0_PREPROCESSED_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("C0-preprocessed fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_C0_PREPROCESSED_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: URL backslash normalization is composed with reversible
/// credential decoding before completed evidence is retained.
#[test]
fn web_search_rejects_decoded_backslash_normalized_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_BACKSLASH_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("decoded backslash fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_DECODED_BACKSLASH_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: URL path dot-segment removal cannot conceal any reversible
/// credential variant in completed result evidence.
#[test]
fn web_search_rejects_dot_segment_normalized_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: format!("{FIXTURE_ORIGIN_ONLY_RESULT_URL}/{URL_DOT_SEGMENT_NORMALIZED_VALUE}"),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("dot-segment-normalized fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_DOT_SEGMENT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: URL preprocessing composes with path dot-segment removal before
/// completed provider result evidence is retained.
#[test]
fn web_search_rejects_preprocessed_dot_segment_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: format!(
            "{FIXTURE_ORIGIN_ONLY_RESULT_URL}/{URL_PREPROCESSED_DOT_SEGMENT_COLLISION_KEY}"
        ),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("preprocessed dot-segment fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_PREPROCESSED_DOT_SEGMENT_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: URL preprocessing composes with authority port normalization
/// before completed provider result evidence is retained.
#[test]
fn web_search_rejects_preprocessed_authority_port_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_INTERNAL_ZERO_PORT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("preprocessed authority-port fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_PREPROCESSED_AUTHORITY_PORT_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: URL preprocessing is closed under reversible decoding before
/// decoded dot segments are removed from completed result URLs.
#[test]
fn web_search_rejects_preprocessed_then_decoded_dot_segment_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: format!(
            "{FIXTURE_ORIGIN_ONLY_RESULT_URL}/{URL_PREPROCESSED_DECODED_DOT_COLLISION_KEY}"
        ),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("preprocessed decoded-dot fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_PREPROCESSED_DECODED_DOT_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: URL path shortening removes the empty segment immediately before
/// a parent segment before completed evidence is retained.
#[test]
fn web_search_rejects_empty_segment_parent_normalized_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: format!("{FIXTURE_ORIGIN_ONLY_RESULT_URL}/{URL_EMPTY_SEGMENT_DOT_COLLISION_KEY}"),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("empty-segment parent fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_EMPTY_SEGMENT_DOT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: Unicode case folding is composed with reversible decoding and
/// URL backslash normalization before completed evidence is retained.
#[test]
fn web_search_rejects_case_folded_decoded_backslash_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_BACKSLASH_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("case-folded decoded backslash fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_DECODED_CASE_BACKSLASH_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: case-insensitive host canonicalization cannot conceal an
/// embedded credential in a provider result host.
#[test]
fn web_search_rejects_embedded_case_normalized_credential_in_result_host() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_EMBEDDED_HOST_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("embedded host fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_EMBEDDED_HOST_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: Unicode host decoding and case normalization cannot conceal an
/// embedded credential in a provider result host.
#[test]
fn web_search_rejects_embedded_unicode_case_normalized_credential_in_result_host() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_UNICODE_EMBEDDED_HOST_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("Unicode host fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_UNICODE_HOST_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: IDNA mapping cannot delete credential code points and leave the
/// canonicalized credential embedded in a provider result host.
#[test]
fn web_search_rejects_embedded_idna_mapped_credential_in_result_host() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IDNA_REMOVED_CODE_POINT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("IDNA-mapped host fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IDNA_REMOVED_CODE_POINT_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: an IDNA compatibility mapping remains detectable when the
/// credential is embedded inside a larger provider result host label.
#[test]
fn web_search_rejects_embedded_idna_compatibility_credential_in_result_host() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IDNA_COMPATIBILITY_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("IDNA compatibility fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IDNA_COMPATIBILITY_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// IP-literal result hosts bypass domain-only IDNA decoding and remain
/// valid structured search evidence.
#[test]
fn web_search_preserves_ipv6_literal_result_host() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("IPv6 fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");

    assert!(success_evidence(response, &scrubber()).is_ok());
}

/// INV-035: equivalent IPv6 spellings cannot conceal a credential in a
/// provider result host after URL canonicalization.
#[test]
fn web_search_rejects_canonicalized_ipv6_credential_in_result_host() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("IPv6 fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV6_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: canonical IPv6 hextet spelling and compression boundaries
/// cannot conceal a credential in a provider result host.
#[test]
fn web_search_rejects_canonicalized_ipv6_hextet_credential_in_result_host() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("IPv6 hextet fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV6_HEXTET_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential that becomes a proper substring of one IPv6 hextet
/// after canonicalization cannot survive in a provider result host.
#[test]
fn web_search_rejects_canonicalized_embedded_ipv6_hextet_credential() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_EMBEDDED_IPV6_HEXTET_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("embedded IPv6 hextet fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_EMBEDDED_IPV6_HEXTET_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential spanning an IPv6 hextet separator and discarded
/// leading zero cannot survive typed host canonicalization.
#[test]
fn web_search_rejects_separator_spanning_ipv6_hextet_credential() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_SEPARATOR_SPANNING_IPV6_HEXTET_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("separator-spanning IPv6 fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV6_SEPARATOR_SPANNING_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a separator-bound zero hextet also accounts for the compressed
/// separator spelling produced by a run of zero IPv6 components.
#[test]
fn web_search_rejects_separator_bound_zero_hextet_compression() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_COMPRESSED_ZERO_HEXTET_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("compressed zero-hextet fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV6_COMPRESSED_ZERO_SEPARATOR_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: adjacent separator-bound zero hextets account for the compressed
/// separator spelling produced by typed IPv6 serialization.
#[test]
fn web_search_rejects_separator_bound_zero_hextet_run_compression() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_COMPRESSED_ZERO_HEXTET_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("compressed zero-run fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV6_COMPRESSED_ZERO_RUN_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a zero run within a larger separator-bound fragment accounts for
/// the compressed spelling produced by typed IPv6 serialization.
#[test]
fn web_search_rejects_separator_bound_zero_run_with_retained_suffix() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_COMPRESSED_ZERO_HEXTET_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("compressed zero-run suffix fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV6_COMPRESSED_ZERO_RUN_SUFFIX_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a multi-hextet credential fragment is canonicalized as one
/// contiguous sequence before comparison with an IPv6 result host.
#[test]
fn web_search_rejects_canonicalized_multi_hextet_credential_in_result_host() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_MULTI_HEXTET_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("multi-hextet fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV6_MULTI_HEXTET_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a bracket adjacent to a partial IPv6 credential fragment is
/// treated as host syntax before comparison with a result host.
#[test]
fn web_search_rejects_canonicalized_bracket_bound_ipv6_fragment() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("bracket-bound IPv6 fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV6_BRACKET_BOUND_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a compressed IPv6 credential fragment is expanded at every
/// legal length and position before comparison with a result host.
#[test]
fn web_search_rejects_canonicalized_compressed_ipv6_credential_fragment() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("compressed-fragment fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV6_COMPRESSED_FRAGMENT_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: canonical hexadecimal rendering of an embedded dotted IPv4
/// tail cannot conceal a credential in an IPv6 provider result host.
#[test]
fn web_search_rejects_canonicalized_ipv4_tail_credential_in_ipv6_result_host() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV4_TAIL_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("embedded IPv4-tail fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV4_TAIL_IPV6_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential embedded within one decimal dotted-IPv4 tail
/// component is compared with that component's canonical hexadecimal output.
#[test]
fn web_search_rejects_ipv4_tail_decimal_digit_substring_in_ipv6_result_host() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV4_TAIL_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("embedded IPv4-tail substring fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV4_TAIL_DIGIT_SUBSTRING_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a separator-bound decimal dotted-IPv4 tail component is compared
/// with that component's canonical hexadecimal output.
#[test]
fn web_search_rejects_separator_bound_ipv4_tail_component_in_ipv6_result_host() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV4_TAIL_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("separator-bound IPv4-tail fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV4_TAIL_SEPARATOR_SUBSTRING_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential spanning dotted-IPv4 tail components is compared
/// with the complete source spelling before typed IPv6 serialization.
#[test]
fn web_search_rejects_cross_component_ipv4_tail_substring() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_DOTTED_IPV4_TAIL_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("cross-component IPv4-tail fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV4_TAIL_CROSS_COMPONENT_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a compressed IPv6 prefix and partial dotted IPv4 tail are
/// canonicalized together before comparison with a result host.
#[test]
fn web_search_rejects_canonicalized_mixed_compressed_ipv6_tail_fragment() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV4_TAIL_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("mixed-tail fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_MIXED_COMPRESSED_IPV6_TAIL_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: an IPv6/IPv4 separator without an explicit IPv6 prefix still
/// canonicalizes its partial dotted tail before result-host comparison.
#[test]
fn web_search_rejects_canonicalized_separator_only_ipv6_tail_fragment() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV4_TAIL_IPV6_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("separator-only mixed-tail fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_SEPARATOR_ONLY_IPV6_TAIL_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: WHATWG legacy IPv4 serialization cannot conceal a credential
/// reflected in a provider result host.
#[test]
fn web_search_rejects_canonicalized_legacy_ipv4_credential_in_result_host() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_LEGACY_IPV4_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("legacy IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_LEGACY_IPV4_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential ending at the permitted trailing dot of a legacy
/// IPv4 host cannot survive typed host canonicalization.
#[test]
fn web_search_rejects_trailing_dot_ipv4_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_TRAILING_DOT_IPV4_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("trailing-dot IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV4_TRAILING_DOT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a legacy octal credential is canonicalized in IPv4 component
/// context before comparison with an already-canonical provider result host.
#[test]
fn web_search_rejects_octal_credential_in_canonical_ipv4_component() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_CANONICAL_IPV4_COMPONENT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("canonical IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_OCTAL_IPV4_COMPONENT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a legacy hexadecimal credential is canonicalized in IPv4
/// component context before comparison with a provider result host.
#[test]
fn web_search_rejects_hex_credential_in_canonical_ipv4_component() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_CANONICAL_IPV4_COMPONENT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("canonical IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_HEX_IPV4_COMPONENT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a provider-added hexadecimal radix prefix cannot conceal a
/// credential that forms the digits of one legacy IPv4 component.
#[test]
fn web_search_rejects_credential_inside_hexadecimal_ipv4_component() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_HEX_AFFIX_IPV4_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("hexadecimal-affix IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_EMBEDDED_HEX_IPV4_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a provider-added octal radix prefix cannot conceal a credential
/// that forms the digits of one legacy IPv4 component.
#[test]
fn web_search_rejects_credential_inside_octal_ipv4_component() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_OCTAL_AFFIX_IPV4_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("octal-affix IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_EMBEDDED_OCTAL_IPV4_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential embedded inside the hexadecimal digits of a legacy
/// IPv4 component cannot survive canonical host serialization.
#[test]
fn web_search_rejects_credential_substring_inside_hexadecimal_ipv4_digits() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_HEX_SUBSTRING_IPV4_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("hexadecimal-substring IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_HEX_DIGIT_SUBSTRING_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential embedded inside the octal digits of a legacy IPv4
/// component cannot survive canonical host serialization.
#[test]
fn web_search_rejects_credential_substring_inside_octal_ipv4_digits() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_OCTAL_SUBSTRING_IPV4_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("octal-substring IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_OCTAL_DIGIT_SUBSTRING_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential spanning a hexadecimal radix marker and its first
/// digit cannot survive canonical host serialization.
#[test]
fn web_search_rejects_credential_spanning_hexadecimal_ipv4_prefix() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_HEX_AFFIX_IPV4_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("hexadecimal-prefix IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_HEX_PREFIX_SPANNING_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential spanning an IPv4 component separator, hexadecimal
/// radix marker, and first digit cannot survive canonical host serialization.
#[test]
fn web_search_rejects_credential_spanning_ipv4_separator_and_hexadecimal_prefix() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_SEPARATOR_SPANNING_HEX_IPV4_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("separator-spanning IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV4_SEPARATOR_SPANNING_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: padding within a separator-bound hexadecimal IPv4 component is
/// normalized before the credential is compared with canonical result output.
#[test]
fn web_search_rejects_padded_separator_bound_hexadecimal_ipv4_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_PADDED_SEPARATOR_SPANNING_HEX_IPV4_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("padded separator-bound IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV4_PADDED_SEPARATOR_SPANNING_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a multi-octet final IPv4 component is preserved as one
/// credential-derived fragment when compared with a canonical result host.
#[test]
fn web_search_rejects_multi_octet_credential_in_canonical_ipv4_component() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_MULTI_OCTET_IPV4_COMPONENT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("multi-octet IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_MULTI_OCTET_IPV4_COMPONENT_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: numeric port canonicalization cannot conceal a credential in
/// a provider result URL.
#[test]
fn web_search_rejects_canonicalized_port_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(URL_PORT_COLLISION_VALUE),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("canonical port fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_PORT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a bare numeric credential is canonicalized in URL port
/// context before provider result evidence is retained.
#[test]
fn web_search_rejects_canonicalized_bare_port_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(URL_PORT_COLLISION_VALUE),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("canonical bare-port fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_BARE_PORT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential normalized to a default HTTP port remains a numeric
/// fragment when it is embedded in another provider result port.
#[test]
fn web_search_rejects_default_port_credential_fragment_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(URL_EMBEDDED_PORT_COLLISION_VALUE),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("embedded port fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_DEFAULT_PORT_FRAGMENT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential spanning the port delimiter and a discarded leading
/// zero cannot survive numeric port canonicalization.
#[test]
fn web_search_rejects_discarded_leading_zero_port_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_LEADING_ZERO_PORT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("leading-zero port fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_DISCARDED_PORT_ZERO_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential spanning an IPv6 authority boundary and a discarded
/// leading port zero cannot survive typed URL canonicalization.
#[test]
fn web_search_rejects_discarded_port_zero_with_ipv6_authority_context() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_IPV6_LEADING_ZERO_PORT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("IPv6 leading-zero port fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_IPV6_AUTHORITY_PORT_ZERO_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: discarded leading port zeros cannot conceal a credential whose
/// retained spelling spans authority context and normalized port digits.
#[test]
fn web_search_rejects_internal_port_zero_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_INTERNAL_ZERO_PORT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("internal port-zero fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_INTERNAL_PORT_ZERO_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: discarded leading port zeros cannot conceal a credential whose
/// retained spelling spans the port/path boundary.
#[test]
fn web_search_rejects_port_path_zero_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_PORT_PATH_ZERO_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("port/path zero fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_PORT_PATH_ZERO_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: discarded leading port zeros cannot conceal a credential whose
/// retained spelling spans authority context, normalized port digits, and
/// the result path.
#[test]
fn web_search_rejects_authority_port_path_zero_credential_in_result_url() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_PORT_PATH_ZERO_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("authority/port/path zero fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_AUTHORITY_PORT_PATH_ZERO_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a maximum-length credential with a zero-prefixed port is reduced
/// directly to its one canonical port/path spelling before evidence admission.
#[test]
fn web_search_bounds_maximum_zero_prefixed_port_path_canonicalization() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: maximum_zero_prefixed_port_path_result_url(),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("maximum zero-prefixed port fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        maximum_zero_prefixed_port_path_collision_key().into_bytes(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: removal of a scheme-default port cannot conceal a credential
/// whose retained spelling spans authority context and the result path.
#[test]
fn web_search_rejects_removed_default_port_path_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_DEFAULT_PORT_PATH_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("default-port path fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_REMOVED_DEFAULT_PORT_PATH_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: removal of a scheme-default port cannot conceal a credential
/// beginning inside the port digits and extending into the result path.
#[test]
fn web_search_rejects_bare_removed_default_port_path_credential() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_DEFAULT_PORT_PATH_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("bare default-port path fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_BARE_REMOVED_DEFAULT_PORT_PATH_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: removal of a scheme-default port cannot conceal a credential
/// beginning inside the removed port digits and extending into the path.
#[test]
fn web_search_rejects_removed_default_port_digit_suffix() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_DEFAULT_PORT_PATH_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("default-port digit-suffix fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_DEFAULT_PORT_DIGIT_SUFFIX_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: port and dot-segment canonicalizers are closed under composition,
/// so their combined transformation cannot conceal a credential.
#[test]
fn web_search_rejects_composed_port_and_dot_segment_normalization() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_COMPOSED_PORT_PATH_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("composed port and path fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_COMPOSED_PORT_PATH_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: removal of a scheme-default port cannot conceal a credential
/// ending at the removed port when the retained authority has no path suffix.
#[test]
fn web_search_rejects_removed_default_port_credential_without_path_suffix() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_DEFAULT_PORT_ONLY_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("default-port-only fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_REMOVED_DEFAULT_PORT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: removing an empty query delimiter records a typed collision fact
/// before the source URL is discarded.
#[test]
fn web_search_rejects_credential_spanning_removed_empty_query_delimiter() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_EMPTY_QUERY_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("empty-query fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_EMPTY_QUERY_DELIMITER_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: dropping parsed user information records a typed collision fact
/// for credentials spanning the user-information and host boundary.
#[test]
fn web_search_rejects_credential_spanning_removed_user_information() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_USER_INFORMATION_BOUNDARY_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("user-information boundary fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_USER_INFORMATION_BOUNDARY_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: dropping explicit empty user information records a typed collision
/// fact for credentials spanning the removed delimiter and host boundary.
#[test]
fn web_search_rejects_credential_spanning_removed_empty_user_information() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_EMPTY_USER_INFORMATION_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("empty user-information fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_EMPTY_USER_INFORMATION_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: dropping an explicit empty port records a typed collision fact for
/// credentials spanning the removed delimiter and retained result path.
#[test]
fn web_search_rejects_credential_spanning_removed_empty_port() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_EMPTY_PORT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("empty-port fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_EMPTY_PORT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: a credential variant retained after user-information removal is
/// passed through host canonicalization before typed evidence is rendered.
#[test]
fn web_search_rejects_removed_user_information_before_ipv4_canonicalization() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_USER_INFORMATION_LEGACY_IPV4_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("userinfo and legacy-IPv4 fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_USER_INFORMATION_LEGACY_IPV4_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: removing a fragment records a typed collision fact before the
/// source URL is discarded.
#[test]
fn web_search_rejects_credential_spanning_removed_fragment_delimiter() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_EMPTY_FRAGMENT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("fragment-boundary fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_EMPTY_FRAGMENT_DELIMITER_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// A credential that parses as a host plus explicit port does not turn the
/// host alone into a collision with an unrelated provider result.
#[test]
fn web_search_preserves_unrelated_host_for_credential_with_explicit_port() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_AUTHORITY_WITH_PORT_NON_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    ))
    .expect("fixture credential is usable");

    assert!(success_evidence(response, &scrubber).is_ok());
}

/// INV-035: complete URL credential variants are canonicalized before
/// comparison with a provider result URL.
#[test]
fn web_search_rejects_canonicalized_complete_url_credential() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_CANONICAL_ORIGIN_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("canonical complete-URL fixture result is admitted");
    let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
        URL_COMPLETE_DEFAULT_PORT_COLLISION_KEY.as_bytes().to_vec(),
    ))
    .expect("fixture credential is usable");

    assert_eq!(
        success_evidence(response, &scrubber),
        Err(WebSearchExecutorError::EvidenceEncoding)
    );
}

/// INV-035: deeply encoded text in an unapproved query parameter is removed
/// structurally, without depending on the scrubber's decode bound.
#[test]
fn web_search_drops_unapproved_query_beyond_the_decode_bound() {
    let reflected = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: format!("{FIXTURE_RESULT_URL}?token={EXCESSIVE_FORM_ENCODING_VALUE}"),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("deeply encoded fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");

    let evidence = success_evidence(response, &scrubber()).expect("typed result encodes");
    let content = completed_text(evidence);

    assert!(!content.contains(EXCESSIVE_FORM_ENCODING_VALUE));
}
