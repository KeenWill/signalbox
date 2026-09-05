use reqwest::Url;
use std::collections::BTreeMap;

use super::{
    diagnostic::*, egress::*, request::*, test_provider_support::*, test_support::*,
    transport_failure::*,
};

/// A supported named HTML reference that does not encode the credential is
/// preserved and does not create a request collision.
#[test]
fn web_search_supported_named_html_reference_does_not_create_credential_collision() {
    let scrubber = scrubber();

    let sanitized = scrubber.redact_text(SAFE_SUPPORTED_NAMED_ENTITY_VALUE);

    assert!(!query_contains_credential(
        SAFE_SUPPORTED_NAMED_ENTITY_VALUE,
        SYNTHETIC_KEY,
    ));
    assert_eq!(sanitized, SAFE_SUPPORTED_NAMED_ENTITY_VALUE);
}

/// The Brave provider mapping owns the exact endpoint and bounded web-only
/// query parameters used by its one physical request.
#[test]
fn brave_request_uses_the_mapped_endpoint_and_parameters() {
    let built = brave_request();
    let endpoint = Url::parse(BRAVE_SEARCH_ENDPOINT).expect("provider endpoint is valid");
    let result_count = provider_result_count_query();
    let parameters = built
        .url()
        .query_pairs()
        .into_owned()
        .collect::<BTreeMap<_, _>>();

    assert_eq!(built.url().scheme(), endpoint.scheme());
    assert_eq!(built.url().host_str(), endpoint.host_str());
    assert_eq!(
        built.url().port_or_known_default(),
        endpoint.port_or_known_default()
    );
    assert_eq!(built.url().path(), endpoint.path());
    assert_eq!(parameters.get("q").map(String::as_str), Some(FIXTURE_QUERY));
    assert_eq!(
        parameters.get("count").map(String::as_str),
        Some(result_count.as_str())
    );
    assert_eq!(
        parameters.get("result_filter").map(String::as_str),
        Some("web")
    );
    assert_eq!(
        parameters.get("text_decorations").map(String::as_str),
        Some("false")
    );
}

/// the API key is header-only, marked sensitive, and absent from
/// both the request URL and its diagnostic rendering.
#[test]
fn brave_request_never_records_credential_in_url_or_debug() {
    let built = brave_request();
    let diagnostic = format!("{built:?}");
    let header = built
        .headers()
        .get(BRAVE_SUBSCRIPTION_TOKEN_HEADER)
        .expect("credential header is present");

    assert!(!built.url().as_str().contains(SYNTHETIC_KEY));
    assert!(header.is_sensitive());
    assert!(!diagnostic.contains(SYNTHETIC_KEY));
}

/// a query containing the resolved API key fails before a request
/// URL can leave the builder or be dispatched.
#[test]
fn brave_request_rejects_query_credential_collision() {
    assert!(matches!(
        build_brave_request(SYNTHETIC_KEY, SYNTHETIC_KEY),
        Err(WebSearchTransportFailure::RequestFailed)
    ));
}

/// canonical Unicode normalization cannot conceal a credential
/// in a query before provider dispatch.
#[test]
fn brave_request_rejects_unicode_normalized_query_credential_collision() {
    let failure = build_brave_request(
        URL_UNICODE_HOST_COLLISION_KEY,
        URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY,
    )
    .expect_err("Unicode-normalized credential query is rejected");

    assert_eq!(
        failure.class(),
        WebSearchTransportFailureClass::RequestFailed
    );
}

/// a reversibly encoded API key in the query fails before URL
/// serialization can double-encode it or dispatch it to the provider.
#[test]
fn brave_request_rejects_percent_encoded_query_credential_collision() {
    assert!(matches!(
        build_brave_request(URL_ENCODED_COLLISION_VALUE, URL_ENCODED_COLLISION_KEY),
        Err(WebSearchTransportFailure::RequestFailed)
    ));
}

/// an HTML-reference spelling of the API key fails before URL
/// construction can dispatch it as provider-controlled query text.
#[test]
fn brave_request_rejects_html_encoded_query_credential_collision() {
    let failure = build_brave_request(HTML_ENTITY_COLLISION_VALUE, HTML_ENTITY_COLLISION_KEY)
        .expect_err("HTML-encoded credential query is rejected");

    assert_eq!(
        failure.class(),
        WebSearchTransportFailureClass::RequestFailed
    );
}

/// repeated HTML character-reference decoding cannot conceal an
/// API key in a query before dispatch.
#[test]
fn brave_request_rejects_nested_html_encoded_query_credential_collision() {
    let failure = build_brave_request(
        HTML_NESTED_ENTITY_COLLISION_VALUE,
        HTML_ENTITY_COLLISION_KEY,
    )
    .expect_err("nested HTML-encoded credential query is rejected");

    assert_eq!(
        failure.class(),
        WebSearchTransportFailureClass::RequestFailed
    );
}

/// composed form and HTML decoding cannot conceal an API key in a
/// query before dispatch.
#[test]
fn brave_request_rejects_form_then_html_encoded_query_credential_collision() {
    let failure = build_brave_request(FORM_HTML_COLLISION_VALUE, HTML_ENTITY_COLLISION_KEY)
        .expect_err("cross-codec credential query is rejected");

    assert_eq!(
        failure.class(),
        WebSearchTransportFailureClass::RequestFailed
    );
}

/// a key matching fixed provider URL text fails before the URL
/// can be dispatched or recorded.
#[test]
fn brave_request_rejects_fixed_url_credential_collision() {
    assert!(matches!(
        build_brave_request(FIXTURE_QUERY, URL_SCHEME_COLLISION_KEY),
        Err(WebSearchTransportFailure::RequestFailed)
    ));
}

/// a key matching fixed request metadata fails before the request
/// diagnostic can leave the transport boundary.
#[test]
fn brave_request_rejects_fixed_header_credential_collision() {
    assert!(matches!(
        build_brave_request(FIXTURE_QUERY, ACCEPT_HEADER_COLLISION_KEY),
        Err(WebSearchTransportFailure::RequestFailed)
    ));
}

/// canonicalized scheme spelling in fixed request metadata cannot
/// conceal the credential before dispatch.
#[test]
fn brave_request_rejects_case_normalized_fixed_scheme_collision() {
    let failure = build_brave_request(FIXTURE_QUERY, URL_SCHEME_CASE_COLLISION_KEY)
        .expect_err("case-normalized fixed scheme collision is rejected");

    assert_eq!(
        failure.class(),
        WebSearchTransportFailureClass::RequestFailed
    );
}

/// canonicalized host spelling in fixed request metadata cannot
/// conceal the credential before dispatch.
#[test]
fn brave_request_rejects_case_normalized_fixed_host_collision() {
    let failure = build_brave_request(FIXTURE_QUERY, PROVIDER_HOST_CASE_COLLISION_KEY)
        .expect_err("case-normalized fixed host collision is rejected");

    assert_eq!(
        failure.class(),
        WebSearchTransportFailureClass::RequestFailed
    );
}

/// canonicalized media-type spelling in fixed request metadata
/// cannot conceal the credential before dispatch.
#[test]
fn brave_request_rejects_case_normalized_fixed_media_type_collision() {
    let failure = build_brave_request(FIXTURE_QUERY, ACCEPT_HEADER_CASE_COLLISION_KEY)
        .expect_err("case-normalized fixed media-type collision is rejected");

    assert_eq!(
        failure.class(),
        WebSearchTransportFailureClass::RequestFailed
    );
}

/// optional HTTP field whitespace cannot alter a credential at
/// the header boundary before dispatch.
#[test]
fn brave_request_rejects_leading_header_whitespace_in_credential() {
    let failure = build_brave_request(FIXTURE_QUERY, LEADING_HEADER_WHITESPACE_KEY)
        .expect_err("boundary-whitespace credential is rejected");

    assert_eq!(
        failure.class(),
        WebSearchTransportFailureClass::InvalidCredential
    );
}

/// a credential beyond the scrubber's inspection bound cannot enter
/// the provider header before dispatch.
#[test]
fn brave_request_rejects_credential_beyond_scrubber_bound() {
    let credential = oversized_provider_credential();

    let failure = build_brave_request(FIXTURE_QUERY, &credential)
        .expect_err("oversized provider credential is rejected");

    assert_eq!(
        failure.class(),
        WebSearchTransportFailureClass::InvalidCredential
    );
}
