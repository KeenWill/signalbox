use reqwest::StatusCode;

use super::{evidence::*, result::*, test_support::*};

/// The structured output retains ten results and reports that the
/// provider returned an omitted eleventh result.
#[test]
fn web_search_result_reports_provider_result_truncation() {
    let response = response_with_result_count(MAX_RETURNED_RESULTS + 1);
    let evidence = success_evidence(response, &scrubber()).expect("response encodes");
    let content = completed_text(evidence);
    let value: serde_json::Value = serde_json::from_str(&content).expect("result is valid JSON");

    assert_eq!(
        value["results"]
            .as_array()
            .expect("results are an array")
            .len(),
        MAX_RETURNED_RESULTS
    );
    assert_eq!(value["truncated"], true);
}

/// Complete provider pagination evidence reports omitted search results
/// even when the current page itself fits the output count.
#[test]
fn web_search_result_reports_provider_pagination_truncation() {
    let response = WebSearchResponse::new(
        vec![result("only result")],
        WebSearchPageCompleteness::MoreAvailable,
    )
    .expect("fixture response is admitted");
    let evidence = success_evidence(response, &scrubber()).expect("response encodes");
    let content = completed_text(evidence);
    let value: serde_json::Value = serde_json::from_str(&content).expect("result is valid JSON");

    assert_eq!(value["truncated"], true);
}

/// A provider result title must retain non-whitespace content.
#[test]
fn web_search_result_rejects_whitespace_only_title() {
    assert!(
        WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_WHITESPACE_TITLE),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .is_none()
    );
}

/// A checked result stores the parser-validated URL serialization rather
/// than provider text discarded during parsing.
#[test]
fn web_search_result_stores_url_text_normalized_by_parser() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_NORMALIZED_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("normalized fixture URL is admitted");

    assert_eq!(result.url(), FIXTURE_RESULT_URL);
}

/// Routine URL canonicalization, including an origin-only trailing slash,
/// is retained as the validated result URL.
#[test]
fn web_search_result_preserves_canonicalizable_origin_url() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_ORIGIN_ONLY_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("origin-only fixture URL is admitted");

    assert_eq!(result.url(), FIXTURE_CANONICAL_ORIGIN_RESULT_URL);
}

/// INV-035: result URL user information is discarded by the typed parser and
/// cannot reach tool evidence.
#[test]
fn web_search_result_drops_parsed_user_information() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_USERINFO_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("userinfo fixture URL is parsed");

    assert_eq!(result.url(), FIXTURE_RESULT_URL);
}

/// INV-035: result URL query parameters not named by the explicit allowlist
/// are discarded before output construction.
#[test]
fn web_search_result_drops_unapproved_query_parameters() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_QUERY_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("query fixture URL is parsed");

    assert_eq!(result.url(), FIXTURE_RESULT_URL);
}

/// INV-035: result URL fragments are discarded before output construction.
#[test]
fn web_search_result_drops_fragments() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_RESULT_TITLE),
        url: String::from(FIXTURE_FRAGMENT_RESULT_URL),
        snippet: String::from(FIXTURE_RESULT_SNIPPET),
    })
    .expect("fragment fixture URL is parsed");

    assert_eq!(result.url(), FIXTURE_RESULT_URL);
}

/// INV-035: provider text is entity-escaped while the response is parsed,
/// before any evidence renderer can observe it.
#[test]
fn web_search_result_entity_escapes_provider_text() {
    let result = WebSearchResult::try_new(WebSearchResultFields {
        title: String::from(FIXTURE_MARKUP_TITLE),
        url: String::from(FIXTURE_RESULT_URL),
        snippet: String::from(FIXTURE_MARKUP_SNIPPET),
    })
    .expect("markup provider-text fixture is bounded");

    assert_eq!(result.title(), FIXTURE_ESCAPED_MARKUP_TITLE);
    assert_eq!(result.snippet(), FIXTURE_ESCAPED_MARKUP_SNIPPET);
}

/// INV-035: provider response and error diagnostics never render
/// provider-controlled fields that could reflect the API key.
#[test]
fn web_search_debug_output_omits_reflected_credential() {
    let fields = WebSearchResultFields {
        title: String::from(SYNTHETIC_KEY),
        url: format!("{FIXTURE_RESULT_URL}?token={SYNTHETIC_KEY}"),
        snippet: String::from(SYNTHETIC_KEY),
    };
    let fields_debug = format!("{fields:?}");
    let reflected = WebSearchResult::try_new(fields).expect("reflected fixture result is admitted");
    let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
        .expect("fixture response is admitted");
    let error =
        WebSearchProviderError::new(PROVIDER_REJECTION_STATUS, SYNTHETIC_KEY.as_bytes().to_vec())
            .expect("fixture error body is bounded");

    assert!(!fields_debug.contains(SYNTHETIC_KEY));
    assert!(!format!("{response:?}").contains(SYNTHETIC_KEY));
    assert!(!format!("{error:?}").contains(SYNTHETIC_KEY));
}

#[test]
fn provider_error_constructor_rejects_non_http_and_success_statuses() {
    assert!(WebSearchProviderError::new(0, Vec::new()).is_none());
    assert!(WebSearchProviderError::new(StatusCode::OK.as_u16(), Vec::new()).is_none());
    assert!(WebSearchProviderError::new(StatusCode::CREATED.as_u16(), Vec::new()).is_none());
    assert!(WebSearchProviderError::new(PROVIDER_REJECTION_STATUS, Vec::new()).is_some());
}
