use std::error::Error;

use reqwest::{Client, StatusCode};
use signalbox_model_runtime::CredentialValue;

use super::{
    diagnostic::*, egress::*, request::*, result::*, test_provider_support::*, test_support::*,
    text_decoding::*, transport::*, transport_failure::*,
};

/// a provider status equal to the API key is retained for
/// request-scoped sanitization but omitted from raw public diagnostics.
#[test]
fn web_search_transport_diagnostics_omit_credential_colliding_status() {
    let status_collision_key = PROVIDER_REJECTION_STATUS.to_string();
    let provider_error = WebSearchProviderError::new(
        PROVIDER_REJECTION_STATUS,
        br#"{"message":"synthetic rejection"}"#.to_vec(),
    )
    .expect("fixture provider error is admitted");

    assert!(!format!("{provider_error:?}").contains(&status_collision_key));
    assert!(!provider_error.to_string().contains(&status_collision_key));

    let failure = WebSearchTransportFailure::ProviderRejected(provider_error);

    assert!(!format!("{failure:?}").contains(&status_collision_key));
    assert!(!failure.to_string().contains(&status_collision_key));
    assert!(failure.source().is_some());
}

/// a successful response whose fixed Debug rendering collides
/// with the request credential is replaced before leaving the transport.
#[test]
fn web_search_transport_rejects_success_diagnostic_credential_collision() {
    const RESPONSE_DIAGNOSTIC_COLLISION_KEY: &str = "Complete";
    let credential = CredentialValue::new(RESPONSE_DIAGNOSTIC_COLLISION_KEY.as_bytes().to_vec());
    let outcome = credential_safe_transport_outcome(Ok(response_with_result_count(1)), &credential);

    assert!(!format!("{outcome:?}").contains(RESPONSE_DIAGNOSTIC_COLLISION_KEY));

    let failure = outcome
        .into_result()
        .expect_err("colliding success diagnostic fails closed");

    assert!(!format!("{failure:?}").contains(RESPONSE_DIAGNOSTIC_COLLISION_KEY));
    assert!(
        !failure
            .to_string()
            .contains(RESPONSE_DIAGNOSTIC_COLLISION_KEY)
    );
    assert!(matches!(
        failure,
        WebSearchTransportFailure::CredentialDiagnosticCollision(_)
    ));
}

/// a transport failure's case-normalized fixed Debug spelling
/// cannot survive in the public transport outcome.
#[test]
fn web_search_transport_rejects_case_normalized_failure_collision() {
    let credential = CredentialValue::new(
        TRANSPORT_CASE_NORMALIZED_FAILURE_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    );
    let outcome =
        WebSearchTransportOutcome::failed(WebSearchTransportFailure::RequestFailed, &credential);

    assert!(!unicode_case_insensitive_contains(
        &format!("{outcome:?}"),
        TRANSPORT_CASE_NORMALIZED_FAILURE_COLLISION_KEY
    ));
    assert!(matches!(
        outcome.into_result(),
        Err(WebSearchTransportFailure::CredentialDiagnosticCollision(_))
    ));
}

/// the public successful transport outcome cannot synthesize a
/// request credential in its outer `Result` diagnostic.
#[test]
fn web_search_transport_rejects_ok_wrapper_credential_collision() {
    const OK_WRAPPER_COLLISION_KEY: &str = "Ok";
    let credential = CredentialValue::new(OK_WRAPPER_COLLISION_KEY.as_bytes().to_vec());
    let outcome = credential_safe_transport_outcome(Ok(response_with_result_count(1)), &credential);

    assert!(!format!("{outcome:?}").contains(OK_WRAPPER_COLLISION_KEY));

    let failure = outcome
        .into_result()
        .expect_err("colliding success wrapper fails closed");

    assert_eq!(
        transport_failure_diagnostic_class(&failure),
        WebSearchCredentialDiagnosticClass::CallerOrHubBug
    );
}

/// the public failed transport outcome cannot synthesize a
/// request credential in its outer `Result` diagnostic and preserves the
/// original failure class.
#[test]
fn web_search_transport_rejects_err_wrapper_credential_collision() {
    const ERR_WRAPPER_COLLISION_KEY: &str = "Err";
    let credential = CredentialValue::new(ERR_WRAPPER_COLLISION_KEY.as_bytes().to_vec());
    let outcome = credential_safe_transport_outcome(
        Err(WebSearchTransportFailure::DispatchUnknown),
        &credential,
    );

    assert!(!format!("{outcome:?}").contains(ERR_WRAPPER_COLLISION_KEY));

    let failure = outcome
        .into_result()
        .expect_err("colliding failure wrapper fails closed");

    assert_eq!(
        transport_failure_diagnostic_class(&failure),
        WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous
    );
}

/// a credential colliding with fixed provider-rejection prose is
/// rejected before that public diagnostic can leave the transport.
#[test]
fn web_search_transport_rejects_credential_colliding_provider_prose() {
    const PROVIDER_PROSE_COLLISION_KEY: &str = "provider";
    let credential = CredentialValue::new(PROVIDER_PROSE_COLLISION_KEY.as_bytes().to_vec());
    let provider_error = WebSearchProviderError::new(
        PROVIDER_REJECTION_STATUS,
        br#"{"message":"synthetic rejection"}"#.to_vec(),
    )
    .expect("fixture provider error is admitted");
    let failure = credential_safe_transport_failure(
        WebSearchTransportFailure::ProviderRejected(provider_error),
        &credential,
    );

    assert!(!format!("{failure:?}").contains(PROVIDER_PROSE_COLLISION_KEY));
    assert!(!failure.to_string().contains(PROVIDER_PROSE_COLLISION_KEY));
    assert!(failure.source().is_none());
    assert!(matches!(
        failure,
        WebSearchTransportFailure::CredentialDiagnosticCollision(_)
    ));
}

/// every transport failure is sanitized against its request key,
/// including a pre-dispatch fixed-URL collision whose error prose overlaps.
#[test]
fn web_search_transport_sanitizes_fixed_url_and_failure_prose_collision() {
    const SEARCH_PROSE_COLLISION_KEY: &str = "search";
    let credential = CredentialValue::new(SEARCH_PROSE_COLLISION_KEY.as_bytes().to_vec());
    let request = WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(FIXTURE_QUERY),
    };
    let client = Client::builder()
        .build()
        .expect("fixture HTTP client builds without network");
    let failure = build_provider_request(&client, &request, &credential)
        .expect_err("fixed endpoint collides with the fixture credential");
    let failure = credential_safe_transport_failure(failure, &credential);

    assert!(!format!("{failure:?}").contains(SEARCH_PROSE_COLLISION_KEY));
    assert!(!failure.to_string().contains(SEARCH_PROSE_COLLISION_KEY));
    assert!(failure.source().is_none());
    assert!(matches!(
        failure,
        WebSearchTransportFailure::CredentialDiagnosticCollision(_)
    ));
}

/// generating a safe transport diagnostic fails closed when the
/// ordinary redaction sentinel itself contains the credential.
#[test]
fn web_search_transport_diagnostic_redaction_overlap_fails_closed() {
    let credential = CredentialValue::new(DIAGNOSTIC_REDACTION_OVERLAP_KEY.as_bytes().to_vec());
    let failure =
        credential_safe_transport_failure(WebSearchTransportFailure::RequestFailed, &credential);

    assert!(!format!("{failure:?}").contains(DIAGNOSTIC_REDACTION_OVERLAP_KEY));
    assert!(
        !failure
            .to_string()
            .contains(DIAGNOSTIC_REDACTION_OVERLAP_KEY)
    );
    assert!(matches!(
        failure,
        WebSearchTransportFailure::CredentialDiagnosticCollision(_)
    ));
}

/// A received non-success status proves provider rejection even when the
/// response body stream subsequently fails; no partial body is retained.
#[test]
fn provider_rejection_survives_incomplete_error_body() {
    let status = StatusCode::TOO_MANY_REQUESTS;
    let failure = finish_provider_response(
        WebSearchProvider::Brave,
        status,
        Err(WebSearchTransportFailure::DispatchUnknown),
    )
    .expect_err("received rejection status is conclusive");
    let error = provider_rejection(failure);

    assert_eq!(error.status, status.as_u16());
    assert!(error.detail.is_none());
    assert_eq!(
        error.body_failure_class,
        Some(WebSearchTransportFailureClass::DispatchUnknown)
    );
}
