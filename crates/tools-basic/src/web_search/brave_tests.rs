use super::{
    brave::*, diagnostic::*, egress::*, test_provider_support::*, test_support::*,
    transport_failure::*,
};

/// The recorded synthetic Brave envelope decodes only web results and the
/// provider's pagination fact; no transport or network is involved.
#[test]
fn brave_recorded_response_decodes_structured_results() {
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "search",
        "query": {
            "original": FIXTURE_QUERY,
            "more_results_available": false,
        },
        "web": {
            "type": "search",
            "results": [{
                "type": "search_result",
                "title": FIXTURE_RESULT_TITLE,
                "url": FIXTURE_RESULT_URL,
                "description": FIXTURE_RESULT_SNIPPET,
            }],
        },
    }))
    .expect("recorded response fixture encodes");

    let response = decode_provider_response(WebSearchProvider::Brave, &body)
        .expect("recorded provider response decodes");
    let decoded = response
        .results()
        .first()
        .expect("recorded response contains its result fixture");

    assert_eq!(decoded.title(), FIXTURE_RESULT_TITLE);
    assert_eq!(decoded.url(), FIXTURE_RESULT_URL);
    assert_eq!(decoded.snippet(), FIXTURE_RESULT_SNIPPET);
    assert!(!response.more_results_available());
}

/// A recorded Brave success envelope with `web: null` is an empty page;
/// the required pagination fact still determines completeness.
#[test]
fn brave_recorded_null_web_response_decodes_empty_results() {
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "search",
        "query": {
            "original": FIXTURE_QUERY,
            "more_results_available": false,
        },
        "web": null,
    }))
    .expect("recorded response fixture encodes");

    let response = decode_provider_response(WebSearchProvider::Brave, &body)
        .expect("recorded empty provider response decodes");

    assert!(response.results().is_empty());
    assert!(!response.more_results_available());
}

/// A success envelope that omits the `web` member is malformed rather than
/// an authoritative empty page.
#[test]
fn brave_response_without_web_member_is_invalid() {
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "search",
        "query": {
            "original": FIXTURE_QUERY,
            "more_results_available": false,
        },
    }))
    .expect("recorded response fixture encodes");

    let failure = decode_provider_response(WebSearchProvider::Brave, &body)
        .expect_err("missing web member is rejected");

    assert_eq!(
        failure.class(),
        WebSearchTransportFailureClass::InvalidResponse
    );
}

/// A success envelope without provider pagination facts cannot claim that
/// its bounded result page is complete.
#[test]
fn brave_response_without_pagination_facts_is_invalid() {
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "search",
        "web": {
            "type": "search",
            "results": [],
        },
    }))
    .expect("recorded response fixture encodes");

    assert!(matches!(
        decode_provider_response(WebSearchProvider::Brave, &body),
        Err(WebSearchTransportFailure::InvalidResponse)
    ));
}

/// A success envelope without a provider result list cannot fabricate an
/// authoritative empty page.
#[test]
fn brave_response_without_result_list_is_invalid() {
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "search",
        "query": {
            "original": FIXTURE_QUERY,
            "more_results_available": false,
        },
        "web": {
            "type": "search",
        },
    }))
    .expect("recorded response fixture encodes");

    assert!(matches!(
        decode_provider_response(WebSearchProvider::Brave, &body),
        Err(WebSearchTransportFailure::InvalidResponse)
    ));
}

/// A structurally compatible `web` object with a different discriminator is
/// not authoritative Brave web-search output.
#[test]
fn brave_response_with_unexpected_web_type_is_invalid() {
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "search",
        "query": {
            "original": FIXTURE_QUERY,
            "more_results_available": false,
        },
        "web": {
            "type": "schema_drift",
            "results": [],
        },
    }))
    .expect("recorded response fixture encodes");

    assert!(matches!(
        decode_provider_response(WebSearchProvider::Brave, &body),
        Err(WebSearchTransportFailure::InvalidResponse)
    ));
}
