use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use signalbox_application::ToolExecutorEvidence;

use super::{
    arguments::*, egress::*, request::*, test_provider_support::*, test_service_support::*,
    test_support::*, test_telemetry_support::*, tool::*,
};

/// One physical query resolves its pinned credential once and dispatches
/// the injected transport once.
#[tokio::test]
async fn web_search_resolves_one_credential_per_physical_request() {
    let resolutions = Arc::new(AtomicUsize::new(0));
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = CountingCredentials {
        resolutions: Arc::clone(&resolutions),
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let supplied = serde_json::json!({"query": FIXTURE_QUERY}).to_string();
    let request = decode_arguments_for_provider(&arguments(&supplied), WebSearchProvider::Brave)
        .expect("fixture request decodes");
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("synthetic transport completes");

    assert!(matches!(evidence, ToolExecutorEvidence::CompletedText(_)));
    assert_eq!(resolutions.load(Ordering::Relaxed), 1);
    assert_eq!(searches.load(Ordering::Relaxed), 1);
}

/// INV-035: query/credential collisions are rejected before the injected
/// transport boundary, independent of the production request builder.
#[tokio::test]
async fn web_search_rejects_query_credential_before_injected_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SYNTHETIC_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(SYNTHETIC_KEY),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("query collision is definitive pre-dispatch evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: fixed provider request metadata is checked before the
/// injected transport boundary.
#[tokio::test]
async fn web_search_rejects_fixed_request_metadata_before_injected_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: ACCEPT_HEADER_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(FIXTURE_QUERY),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("fixed metadata collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: every fixed provider query component is checked before the
/// injected transport boundary.
#[tokio::test]
async fn web_search_rejects_fixed_query_metadata_before_injected_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: FIXED_QUERY_PARAMETER_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(FIXTURE_QUERY),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("fixed query metadata collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: syntax introduced by serializing the provider URL is checked
/// before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_serialized_url_syntax_before_injected_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SERIALIZED_URL_SYNTAX_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(FIXTURE_QUERY),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("serialized URL collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: the request's fixed Debug representation is checked before the
/// injected transport boundary.
#[tokio::test]
async fn web_search_rejects_fixed_request_debug_before_injected_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: REQUEST_DEBUG_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(FIXTURE_QUERY),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("fixed request Debug collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: the credential value's fixed redacted Debug representation is
/// checked before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_fixed_credential_debug_before_injected_transport() {
    let output = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_writer(output.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: CREDENTIAL_DEBUG_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(FIXTURE_QUERY),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("credential Debug collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
    assert!(output.text().is_empty());
}

/// INV-035: the result's fixed redacted Debug representation is checked
/// before an injected transport can emit it as telemetry.
#[tokio::test]
async fn web_search_rejects_fixed_result_debug_before_injected_transport() {
    let output = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_writer(output.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: RESULT_DEBUG_COLLISION_KEY,
    };
    let transport = ResultDebugFormattingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("fixed result Debug collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
    assert!(output.text().is_empty());
}

/// INV-035: diagnostic framing that combines the request and credential
/// Debug values is checked before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_combined_request_credential_debug_before_transport() {
    let output = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_writer(output.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: REQUEST_CREDENTIAL_DEBUG_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("combined Debug collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
    assert!(output.text().is_empty());
}

/// INV-035: every bounded response Debug representation is checked before
/// the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_fixed_response_debug_before_injected_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: RESPONSE_DEBUG_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(FIXTURE_QUERY),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("fixed response Debug collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: fixed successful-payload member names are checked before the
/// injected transport boundary.
#[tokio::test]
async fn web_search_rejects_fixed_success_payload_before_injected_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SUCCESS_PAYLOAD_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(FIXTURE_QUERY),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("fixed success-payload collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: fixed JSON delimiters in every successful payload are checked
/// before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_fixed_success_payload_delimiter_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SUCCESS_PAYLOAD_DELIMITER_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(FIXTURE_QUERY),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("fixed payload delimiter collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: the empty result-list representation is checked before the
/// injected transport boundary.
#[tokio::test]
async fn web_search_rejects_empty_success_payload_before_injected_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SUCCESS_PAYLOAD_EMPTY_RESULTS_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("empty result-list collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: the delimiter between multiple serialized results is checked
/// before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_multi_result_payload_before_injected_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SUCCESS_PAYLOAD_MULTI_RESULT_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("multi-result collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: fixed successful-payload field framing is checked together
/// with every possible provider-controlled value prefix before dispatch.
#[tokio::test]
async fn web_search_rejects_success_field_boundary_collision_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SUCCESS_FIELD_BOUNDARY_COLLISION_KEY,
    };
    let transport = SuccessFieldBoundaryTransport {
        searches: Arc::clone(&searches),
        title: SUCCESS_FIELD_BOUNDARY_COLLISION_VALUE,
        snippet: FIXTURE_RESULT_SNIPPET,
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("success-field boundary collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: every provider-controlled value suffix is checked together
/// with the following fixed successful-payload framing before dispatch.
#[tokio::test]
async fn web_search_rejects_trailing_success_field_boundary_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SUCCESS_FIELD_TRAILING_BOUNDARY_COLLISION_KEY,
    };
    let transport = SuccessFieldBoundaryTransport {
        searches: Arc::clone(&searches),
        title: SUCCESS_FIELD_TRAILING_BOUNDARY_COLLISION_VALUE,
        snippet: FIXTURE_RESULT_SNIPPET,
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("trailing success-field boundary collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: the actual serialized snippet-to-title boundary is checked
/// against every possible provider-controlled snippet suffix before dispatch.
#[tokio::test]
async fn web_search_rejects_serialized_snippet_title_boundary_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SUCCESS_SNIPPET_TITLE_BOUNDARY_COLLISION_KEY,
    };
    let transport = SuccessFieldBoundaryTransport {
        searches: Arc::clone(&searches),
        title: FIXTURE_RESULT_TITLE,
        snippet: SUCCESS_SNIPPET_TITLE_BOUNDARY_COLLISION_VALUE,
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("snippet-to-title boundary collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: a credential split across two provider-controlled values and
/// their complete serialized field separator is rejected before dispatch.
#[tokio::test]
async fn web_search_rejects_dynamic_field_boundary_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SUCCESS_DYNAMIC_FIELD_BOUNDARY_COLLISION_KEY,
    };
    let transport = SuccessFieldBoundaryTransport {
        searches: Arc::clone(&searches),
        title: SUCCESS_DYNAMIC_FIELD_BOUNDARY_TITLE,
        snippet: SUCCESS_DYNAMIC_FIELD_BOUNDARY_SNIPPET,
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("dynamic field-boundary collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: a credential split across adjacent provider results and their
/// complete serialized result separator is rejected before dispatch.
#[tokio::test]
async fn web_search_rejects_dynamic_result_boundary_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SUCCESS_DYNAMIC_RESULT_BOUNDARY_COLLISION_KEY,
    };
    let transport = SuccessResultBoundaryTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("dynamic result-boundary collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: fixed bound-wrapper and payload prefixes are checked together
/// with every possible provider-controlled value prefix before dispatch.
#[tokio::test]
async fn web_search_rejects_bound_wrapper_dynamic_prefix_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: BOUND_WRAPPER_DYNAMIC_PREFIX_COLLISION_KEY,
    };
    let transport = SuccessFieldBoundaryTransport {
        searches: Arc::clone(&searches),
        title: FIXTURE_RESULT_TITLE,
        snippet: BOUND_WRAPPER_DYNAMIC_PREFIX_COLLISION_VALUE,
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("bound-wrapper prefix collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: an HTML C1 numeric reference is decoded through its standard
/// replacement mapping before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_c1_numeric_reference_credential_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: HTML_NUMERIC_C1_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(HTML_NUMERIC_C1_COLLISION_VALUE),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("C1 numeric-reference collision is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: an unimplemented standard named HTML reference fails closed
/// before query text reaches the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_unsupported_named_html_reference_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: UNSUPPORTED_NAMED_ENTITY_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(UNSUPPORTED_NAMED_ENTITY_COLLISION_VALUE),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("unsupported named-reference syntax is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: semicolonless numeric HTML references fail closed before query
/// text reaches the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_semicolonless_numeric_reference_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SEMICOLONLESS_NUMERIC_HTML_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(SEMICOLONLESS_NUMERIC_HTML_COLLISION_VALUE),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("semicolonless numeric-reference syntax is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: legacy named HTML references that allow an omitted semicolon
/// fail closed before query text reaches the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_semicolonless_named_reference_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: SEMICOLONLESS_NAMED_HTML_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(SEMICOLONLESS_NAMED_HTML_COLLISION_VALUE),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("semicolonless named-reference syntax is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: a numeric HTML reference whose terminator exceeds the scan
/// window fails closed before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_over_window_numeric_reference_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: OVER_WINDOW_NUMERIC_HTML_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: over_window_numeric_html_reflection(),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("over-window numeric reference is definitive evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: reversible query decoding and Unicode case normalization
/// cannot conceal a credential before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_encoded_case_normalized_query_credential_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: QUERY_CASE_NORMALIZED_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(QUERY_CASE_NORMALIZED_COLLISION_VALUE),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("encoded case-normalized collision is definitive evidence");
    let _detail = known_failure_detail(evidence);

    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: reversible decoding of the credential itself cannot conceal a
/// collision with a decoded query before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_query_matching_decoded_credential_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: REVERSE_ENCODED_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(REVERSE_ENCODED_COLLISION_VALUE),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("decoded credential collision is definitive pre-dispatch evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: JSON Unicode escapes cannot conceal a credential before the
/// injected transport boundary.
#[tokio::test]
async fn web_search_rejects_json_unicode_escaped_query_credential_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: JSON_UNICODE_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(JSON_UNICODE_COLLISION_VALUE),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("JSON-escaped credential collision is definitive evidence");
    let _detail = known_failure_detail(evidence);

    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: reversible short JSON escapes in the credential itself cannot
/// conceal a decoded query before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_query_matching_json_solidus_escaped_credential() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: JSON_SOLIDUS_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(JSON_SOLIDUS_COLLISION_VALUE),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("JSON solidus collision is definitive pre-dispatch evidence");

    assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// INV-035: a multi-character full Unicode case fold cannot conceal a
/// credential before the injected transport boundary.
#[tokio::test]
async fn web_search_rejects_full_case_folded_query_credential_before_transport() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: UNICODE_FULL_FOLD_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(UNICODE_FULL_FOLD_COLLISION_VALUE),
    };
    let correlation = dispatch_correlation();

    let evidence = executor
        .execute_request(request, &correlation)
        .await
        .into_result()
        .expect("full-fold credential collision is definitive evidence");
    let _detail = known_failure_detail(evidence);

    assert_eq!(searches.load(Ordering::Relaxed), 0);
}
