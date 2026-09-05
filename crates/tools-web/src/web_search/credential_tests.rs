use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_domain::ToolExecutionErrorDetail;
use signalbox_model_runtime::CredentialValue;

use super::{
    diagnostic::*, evidence::*, redaction::*, test_provider_support::*, test_service_support::*,
    test_support::*, tool::*,
};

/// sanitizing a dispatch-unknown diagnostic preserves its
/// commit-ambiguous classification through a credential-safe executor error.
#[tokio::test]
async fn web_search_sanitized_dispatch_unknown_stays_commit_ambiguous() {
    const UNKNOWN_PROSE_COLLISION_KEY: &str = "unknown";
    let credentials = StaticCredentials {
        value: UNKNOWN_PROSE_COLLISION_KEY,
    };
    let (_catalog, mut executor) = WebSearchTool::try_new(
        credentials,
        SanitizedDispatchUnknownTransport,
        configuration(),
    )
    .expect("fixture web_search tool compiles")
    .into_parts();
    let correlation = dispatch_correlation();
    let executor_error = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect_err("dispatch remains ambiguous");

    assert!(!format!("{executor_error:?}").contains(UNKNOWN_PROSE_COLLISION_KEY));
    assert!(
        !executor_error
            .to_string()
            .contains(UNKNOWN_PROSE_COLLISION_KEY)
    );
    assert_eq!(
        executor_error.operator_failure_class(),
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: true
        }
    );
}

/// credential collisions preserve definitive invalid-credential
/// evidence without retaining the colliding diagnostic detail.
#[test]
fn credential_collision_retains_invalid_credential_evidence() {
    let detail = colliding_failure_detail(
        "InvalidCredential",
        WebSearchTransportFailureClass::InvalidCredential,
    );

    assert_eq!(detail, CREDENTIAL_UNAVAILABLE_DETAIL);
}

/// credential collisions preserve definitive request-failure
/// evidence without retaining the colliding diagnostic detail.
#[test]
fn credential_collision_retains_request_failure_evidence() {
    let detail = colliding_failure_detail(
        "RequestFailed",
        WebSearchTransportFailureClass::RequestFailed,
    );

    assert_eq!(detail, REQUEST_FAILED_DETAIL);
}

/// credential collisions preserve definitive provider-rejection
/// evidence without retaining the colliding diagnostic detail.
#[test]
fn credential_collision_retains_provider_rejection_evidence() {
    let detail = colliding_failure_detail(
        "ProviderRejected",
        WebSearchTransportFailureClass::ProviderRejected,
    );

    assert_eq!(detail, PROVIDER_REJECTED_DETAIL);
}

/// credential collisions preserve definitive invalid-response
/// evidence without retaining the colliding diagnostic detail.
#[test]
fn credential_collision_retains_invalid_response_evidence() {
    let detail = colliding_failure_detail(
        "InvalidResponse",
        WebSearchTransportFailureClass::InvalidResponse,
    );

    assert_eq!(detail, INVALID_RESPONSE_DETAIL);
}

/// credential collisions preserve definitive response-overflow
/// evidence without retaining the colliding diagnostic detail.
#[test]
fn credential_collision_retains_response_overflow_evidence() {
    let detail = colliding_failure_detail(
        "ResponseTooLarge",
        WebSearchTransportFailureClass::ResponseTooLarge,
    );

    assert_eq!(detail, INVALID_RESPONSE_DETAIL);
}

/// a definitive failure detail that collides with the credential
/// is omitted without converting the failure into an executor error.
#[test]
fn credential_collision_omits_known_failure_detail() {
    let credential = CredentialValue::new(REQUEST_DETAIL_COLLISION_KEY.as_bytes().to_vec());
    let scrubber = CredentialScrubber::try_new(&credential).expect("fixture key is usable");
    let detail = ToolExecutionErrorDetail::try_new(String::from(REQUEST_FAILED_DETAIL))
        .expect("fixture failure detail is valid");

    let evidence = known_failure_evidence(detail, &scrubber)
        .expect("detail collision preserves definitive evidence");

    assert_eq!(known_failure_detail(evidence), None);
}

/// a colliding definitive request-failure detail commits through
/// the public service path before dispatch rather than invoking crash
/// classification.
#[tokio::test]
async fn credential_collision_commits_request_failure_without_crash() {
    let (outcome, searches) =
        execute_request_failure_through_service(REQUEST_DETAIL_COLLISION_KEY).await;

    assert!(is_committed_known_failure(&outcome));
    assert_eq!(searches, 0);
}

/// a case-normalized definitive detail collision is omitted while
/// the public service commits a pre-dispatch known failure.
#[tokio::test]
async fn case_normalized_detail_collision_commits_request_failure() {
    let (outcome, searches) =
        execute_request_failure_through_service(CASE_NORMALIZED_REQUEST_DETAIL_COLLISION_KEY).await;

    assert!(is_committed_known_failure_without_detail(&outcome));
    assert_eq!(searches, 0);
}

/// a dynamic provider-rejection detail that collides with the
/// credential is omitted while the public service commits known failure.
#[tokio::test]
async fn credential_collision_commits_provider_rejection_without_crash() {
    let (outcome, searches) =
        execute_provider_rejection_through_service(ProviderStatusCredentials).await;

    assert!(is_committed_known_failure_without_detail(&outcome));
    assert_eq!(searches, 1);
}

/// a credential spanning the populated-detail wrapper and a
/// dynamic provider rejection falls back to definitive detail-free evidence.
#[tokio::test]
async fn wrapper_spanning_provider_detail_collision_commits_known_failure() {
    let credentials = StaticCredentials {
        value: DYNAMIC_PROVIDER_REJECTION_WRAPPER_COLLISION_KEY,
    };

    let (outcome, searches) = execute_provider_rejection_through_service(credentials).await;

    assert!(is_committed_known_failure_without_detail(&outcome));
    assert_eq!(searches, 1);
}

/// a post-response sanitization failure is definitive invalid
/// response evidence rather than a dispatch-ambiguous executor error.
#[tokio::test]
async fn web_search_post_response_sanitization_failure_is_known_failed() {
    const REFLECTED_TITLE_COLLISION_KEY: &str = "reflected-title-secret";
    let credentials = StaticCredentials {
        value: REFLECTED_TITLE_COLLISION_KEY,
    };
    let (_catalog, mut executor) =
        WebSearchTool::try_new(credentials, ReflectedTitleTransport, configuration())
            .expect("fixture web_search tool compiles")
            .into_parts();
    let correlation = dispatch_correlation();
    let evidence = executor
        .execute_request(request(), &correlation)
        .await
        .into_result()
        .expect("completed response sanitization has a definitive outcome");
    let detail = known_failure_detail(evidence).expect("invalid response detail is safe");

    assert!(!detail.contains(REFLECTED_TITLE_COLLISION_KEY));
    assert_eq!(detail, INVALID_RESPONSE_DETAIL);
}

/// An empty credential is a definitive pre-dispatch known failure, not an
/// executor crash classification.
#[tokio::test]
async fn empty_credential_value_commits_known_failure_without_dispatch() {
    let (outcome, searches) = execute_raw_credential_through_service(EMPTY_CREDENTIAL_VALUE).await;

    assert!(is_committed_known_failure(&outcome));
    assert_eq!(searches, 0);
}

/// A non-UTF-8 credential is a definitive pre-dispatch known failure, not
/// an executor crash classification.
#[tokio::test]
async fn non_utf8_credential_value_commits_known_failure_without_dispatch() {
    let (outcome, searches) =
        execute_raw_credential_through_service(NON_UTF8_CREDENTIAL_VALUE).await;

    assert!(is_committed_known_failure(&outcome));
    assert_eq!(searches, 0);
}

/// an interior HTTP-header-invalid byte is a definitive
/// pre-dispatch failure and never reaches the injected transport.
#[tokio::test]
async fn interior_newline_credential_commits_without_dispatch() {
    let (outcome, searches) =
        execute_raw_credential_through_service(INTERIOR_NEWLINE_CREDENTIAL_VALUE).await;

    assert!(is_committed_known_failure(&outcome));
    assert_eq!(searches, 0);
}

/// a credential at the byte bound crosses all boundary preflights
/// without repeated normalization and remains usable.
#[tokio::test]
async fn credential_at_byte_bound_reaches_transport_with_linear_scan() {
    let credential = vec![b'x'; MAX_CREDENTIAL_BYTES];

    let (outcome, searches) = execute_raw_credential_through_service(&credential).await;

    assert!(is_committed_completed(&outcome));
    assert_eq!(searches, 1);
}

/// a credential with trailing HTTP field whitespace commits
/// definitive pre-dispatch evidence without reaching injected transport.
#[tokio::test]
async fn trailing_header_whitespace_credential_commits_without_dispatch() {
    let (outcome, searches) =
        execute_raw_credential_through_service(TRAILING_HEADER_WHITESPACE_KEY).await;

    assert!(is_committed_known_failure(&outcome));
    assert_eq!(searches, 0);
}
