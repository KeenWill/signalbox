use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use signalbox_application::{
    InProcessToolDispatchGate, ToolExecutionService, UuidV7ToolLoopIdGenerator,
};
use signalbox_model_runtime::CredentialValue;

use super::{
    binding::*, redaction::*, test_provider_support::*, test_service_support::*, test_support::*,
    test_telemetry_support::*, text_decoding::*, tool::*,
};

/// the actual `ToolExecutor::execute` result rejects a fixed evidence
/// label before dispatch and never reproduces the request credential.
#[tokio::test]
async fn web_search_bound_executor_result_omits_credential_collision() {
    let diagnostic = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&diagnostic);
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: EXECUTOR_OUTCOME_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let executor = FormattingExecutor {
        inner: executor,
        diagnostic: captured,
    };
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );

    let result = service.execute(batch.session(), batch.turn()).await;
    let rendered = diagnostic
        .lock()
        .expect("captured executor diagnostic lock is available")
        .clone();

    assert!(result.is_ok());
    assert!(!rendered.contains(EXECUTOR_OUTCOME_COLLISION_KEY));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// case normalization of fixed bound-evidence Debug tokens rejects a
/// request credential before dispatch without reproducing it.
#[tokio::test]
async fn web_search_bound_executor_result_omits_case_normalized_credential_collision() {
    let diagnostic = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&diagnostic);
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: EXECUTOR_CASE_NORMALIZED_OUTCOME_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let executor = FormattingExecutor {
        inner: executor,
        diagnostic: captured,
    };
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );

    let result = service.execute(batch.session(), batch.turn()).await;
    let rendered = diagnostic
        .lock()
        .expect("captured executor diagnostic lock is available")
        .clone();

    assert!(result.is_ok());
    assert!(!unicode_case_insensitive_contains(
        &rendered,
        EXECUTOR_CASE_NORMALIZED_OUTCOME_COLLISION_KEY
    ));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// fixed bound-wrapper vocabulary is checked before physical
/// dispatch, not only after evidence is correlated.
#[tokio::test]
async fn web_search_bound_wrapper_collision_fails_before_dispatch() {
    let (failed, searches, rendered) = execute_formatted_raw_credential_through_service(
        EXECUTOR_BOUND_WRAPPER_COLLISION_KEY.as_bytes(),
    )
    .await;

    assert!(failed);
    assert!(!unicode_case_insensitive_contains(
        &rendered,
        EXECUTOR_BOUND_WRAPPER_COLLISION_KEY,
    ));
    assert_eq!(searches, 0);
}

/// exact field framing introduced by the correlated bound wrapper
/// is checked before physical dispatch.
#[tokio::test]
async fn web_search_exact_bound_wrapper_framing_fails_before_dispatch() {
    let (failed, searches, rendered) = execute_formatted_raw_credential_through_service(
        EXECUTOR_BOUND_WRAPPER_FIELD_COLLISION_KEY.as_bytes(),
    )
    .await;

    assert!(failed);
    assert!(!rendered.contains(EXECUTOR_BOUND_WRAPPER_FIELD_COLLISION_KEY));
    assert_eq!(searches, 0);
}

/// populated completed evidence is combined with the correlated
/// bound wrapper before physical dispatch.
#[tokio::test]
async fn web_search_populated_success_wrapper_fails_before_dispatch() {
    let (failed, searches, rendered) = execute_formatted_raw_credential_through_service(
        EXECUTOR_POPULATED_SUCCESS_WRAPPER_COLLISION_KEY.as_bytes(),
    )
    .await;

    assert!(failed);
    assert!(!rendered.contains(EXECUTOR_POPULATED_SUCCESS_WRAPPER_COLLISION_KEY));
    assert_eq!(searches, 0);
}

/// boundary whitespace cannot hide a credential that normalizes
/// to fixed bound-evidence vocabulary.
#[tokio::test]
async fn web_search_bound_result_checks_trimmed_unusable_credential() {
    let credential = std::str::from_utf8(BOUNDARY_WHITESPACE_BOUND_COLLISION_KEY)
        .expect("fixture credential is UTF-8");
    let trimmed = credential.trim();

    let (failed, searches, rendered) =
        execute_formatted_raw_credential_through_service(BOUNDARY_WHITESPACE_BOUND_COLLISION_KEY)
            .await;

    assert!(failed);
    assert!(!unicode_case_insensitive_contains(&rendered, trimmed));
    assert_eq!(searches, 0);
}

/// the definitive `KnownFailed` vocabulary never exempts a matching
/// request credential from the final correlated evidence check.
#[tokio::test]
async fn web_search_bound_failure_word_collision_fails_before_dispatch() {
    let (failed, searches, rendered) = execute_formatted_raw_credential_through_service(
        EXECUTOR_KNOWN_FAILURE_WORD_COLLISION_KEY.as_bytes(),
    )
    .await;

    assert!(failed);
    assert!(!unicode_case_insensitive_contains(
        &rendered,
        EXECUTOR_KNOWN_FAILURE_WORD_COLLISION_KEY,
    ));
    assert_eq!(searches, 0);
}

/// a credential matching the fixed `KnownFailed` Debug token is
/// rejected before dispatch and omitted from the public executor result.
#[tokio::test]
async fn web_search_bound_known_failure_token_omits_case_folded_credential_collision() {
    let diagnostic = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&diagnostic);
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: EXECUTOR_KNOWN_FAILURE_TOKEN_COLLISION_KEY,
    };
    let transport = RequestFailedTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let executor = FormattingExecutor {
        inner: executor,
        diagnostic: captured,
    };
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );

    let result = service.execute(batch.session(), batch.turn()).await;
    let rendered = diagnostic
        .lock()
        .expect("captured executor diagnostic lock is available")
        .clone();

    assert!(result.is_err());
    assert!(!unicode_case_insensitive_contains(
        &rendered,
        EXECUTOR_KNOWN_FAILURE_TOKEN_COLLISION_KEY,
    ));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// a credential matching a substring of the fixed `KnownFailed`
/// Debug token is rejected before dispatch and omitted from the public
/// executor result.
#[tokio::test]
async fn web_search_bound_known_failure_token_omits_credential_substring_collision() {
    let diagnostic = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&diagnostic);
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: EXECUTOR_KNOWN_FAILURE_SUBSTRING_COLLISION_KEY,
    };
    let transport = RequestFailedTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let executor = FormattingExecutor {
        inner: executor,
        diagnostic: captured,
    };
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );

    let result = service.execute(batch.session(), batch.turn()).await;
    let rendered = diagnostic
        .lock()
        .expect("captured executor diagnostic lock is available")
        .clone();

    assert!(result.is_err());
    assert!(!unicode_case_insensitive_contains(
        &rendered,
        EXECUTOR_KNOWN_FAILURE_SUBSTRING_COLLISION_KEY,
    ));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// fixed populated-detail framing is rejected during credential
/// admission and preserves definitive credential-unavailable evidence.
#[tokio::test]
async fn web_search_populated_failure_detail_collision_commits_before_dispatch() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: EXECUTOR_POPULATED_FAILURE_COLLISION_KEY,
    };
    let transport = ProviderRejectedTransport {
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
        .expect("populated detail collision is definitive pre-dispatch evidence");

    assert_eq!(
        known_failure_detail(evidence).as_deref(),
        Some(CREDENTIAL_UNAVAILABLE_DETAIL)
    );
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// every populated fixed failure detail is checked inside the
/// complete bound wrapper before dispatch and falls back to detail-free evidence.
#[tokio::test]
async fn web_search_invalid_response_detail_collision_commits_before_dispatch() {
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: EXECUTOR_INVALID_RESPONSE_POPULATED_COLLISION_KEY,
    };
    let transport = InvalidResponseTransport {
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
        .expect("invalid-response detail collision is definitive pre-dispatch evidence");

    assert_eq!(known_failure_detail(evidence), None);
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// the outer `Ok` wrapper is included in case-normalized checks
/// of the complete public known-failure executor result.
#[tokio::test]
async fn web_search_bound_known_failure_omits_outer_ok_wrapper_collision() {
    let diagnostic = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&diagnostic);
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: EXECUTOR_OK_WRAPPER_COLLISION_KEY,
    };
    let transport = RequestFailedTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let executor = FormattingExecutor {
        inner: executor,
        diagnostic: captured,
    };
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );

    let result = service.execute(batch.session(), batch.turn()).await;
    let rendered = diagnostic
        .lock()
        .expect("captured executor diagnostic lock is available")
        .clone();

    assert!(result.is_err());
    assert!(!unicode_case_insensitive_contains(
        &rendered,
        EXECUTOR_OK_WRAPPER_COLLISION_KEY,
    ));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// punctuation does not conceal a case-normalized fixed Debug
/// spelling during pre-dispatch credential validation.
#[tokio::test]
async fn web_search_bound_executor_result_omits_punctuated_case_collision() {
    let diagnostic = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&diagnostic);
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: EXECUTOR_PUNCTUATED_OUTCOME_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let executor = FormattingExecutor {
        inner: executor,
        diagnostic: captured,
    };
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );

    let result = service.execute(batch.session(), batch.turn()).await;
    let rendered = diagnostic
        .lock()
        .expect("captured executor diagnostic lock is available")
        .clone();

    assert!(result.is_ok());
    assert!(!unicode_case_insensitive_contains(
        &rendered,
        EXECUTOR_PUNCTUATED_OUTCOME_COLLISION_KEY
    ));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// a credential that can collide with the fixed outer `Err`
/// marker is rejected before physical dispatch, and the resulting complete
/// executor diagnostic does not reproduce it.
#[tokio::test]
async fn web_search_bound_executor_error_result_omits_credential_collision() {
    let diagnostic = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&diagnostic);
    let searches = Arc::new(AtomicUsize::new(0));
    let credentials = StaticCredentials {
        value: EXECUTOR_ERROR_COLLISION_KEY,
    };
    let transport = CountingTransport {
        searches: Arc::clone(&searches),
    };
    let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
        .expect("static web_search tool compiles")
        .into_parts();
    let executor = FormattingExecutor {
        inner: executor,
        diagnostic: captured,
    };
    let batch = prepared_web_search_batch();
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        executor_fixture_transaction(batch.clone()),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );

    let result = service.execute(batch.session(), batch.turn()).await;
    let rendered = diagnostic
        .lock()
        .expect("captured executor diagnostic lock is available")
        .clone();

    assert!(result.is_ok(), "unexpected service result: {result:?}");
    assert!(!rendered.contains(EXECUTOR_ERROR_COLLISION_KEY));
    assert_eq!(searches.load(Ordering::Relaxed), 0);
}

/// an oversized resolved credential is a definitive pre-dispatch
/// failure and is never expanded by the scrubber or sent to transport.
#[tokio::test]
async fn oversized_credential_commits_without_dispatch() {
    let credential = vec![b'x'; MAX_CREDENTIAL_BYTES + 1];

    let (outcome, searches) = execute_raw_credential_through_service(&credential).await;

    assert!(is_committed_known_failure(&outcome));
    assert_eq!(searches, 0);
}

/// a credential beyond the bounded inspection budget fails
/// closed before whole-value normalization, decoding, or dispatch.
#[tokio::test]
async fn oversized_credential_above_inspection_bound_fails_closed() {
    let credential = vec![b'x'; MAX_OVERSIZED_CREDENTIAL_INSPECTION_BYTES + 1];

    let (failed, searches, rendered) =
        execute_formatted_raw_credential_through_service(&credential).await;

    assert!(failed);
    assert_eq!(searches, 0);
    assert!(!rendered.contains(std::str::from_utf8(&credential).expect("fixture is UTF-8")));
}

/// oversized credential values cannot reach telemetry through a
/// bounded reversible decoding chain.
#[test]
fn oversized_encoded_credential_value_diagnostic_is_suppressed() {
    let encoded_once = fully_percent_encode(OVERSIZED_CREDENTIAL_TELEMETRY_COLLISION_VALUE);
    let encoded_twice = fully_percent_encode(&encoded_once);
    let encoded_thrice = fully_percent_encode(&encoded_twice);
    let encoded_four_times = fully_percent_encode(&encoded_thrice);
    let correlation = dispatch_correlation();
    let credential = CredentialValue::new(encoded_four_times.as_bytes().to_vec());

    let (diagnostic, result) = capture_credential_value_failure(&correlation, &credential);
    let report_error = result.expect_err("oversized credential telemetry is suppressed");

    assert!(encoded_four_times.len() > MAX_CREDENTIAL_BYTES);
    assert!(diagnostic.is_empty());
    assert!(!format!("{report_error:?}").contains(&encoded_four_times));
    assert!(!format!("{report_error:?}").contains(OVERSIZED_CREDENTIAL_TELEMETRY_COLLISION_VALUE));
}

/// bounded reversible variants of an oversized credential remain
/// checked against the public executor result.
#[tokio::test]
async fn oversized_encoded_bound_wrapper_collision_is_suppressed() {
    let encoded_once = fully_percent_encode(OVERSIZED_BOUND_WRAPPER_COLLISION_VALUE);
    let encoded_twice = fully_percent_encode(&encoded_once);
    let encoded_thrice = fully_percent_encode(&encoded_twice);
    let encoded_four_times = fully_percent_encode(&encoded_thrice);

    let (failed, searches, rendered) =
        execute_formatted_raw_credential_through_service(encoded_four_times.as_bytes()).await;

    assert!(encoded_four_times.len() > MAX_CREDENTIAL_BYTES);
    assert!(failed);
    assert_eq!(searches, 0);
    assert!(!rendered.contains(&encoded_four_times));
    assert!(!rendered.contains(OVERSIZED_BOUND_WRAPPER_COLLISION_VALUE));
}
