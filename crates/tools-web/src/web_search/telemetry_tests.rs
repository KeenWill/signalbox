use signalbox_model_runtime::{
    CredentialAccessError, CredentialAccessFailure, CredentialReference, CredentialValue,
};

use super::{
    diagnostic::*, egress::*, test_provider_support::*, test_service_support::*, test_support::*,
    test_telemetry_support::*, text_decoding::*, transport_failure::*,
};

/// credential-resolution telemetry carries only its safe closed
/// classification and request correlation.
#[test]
fn credential_failure_diagnostic_preserves_safe_classification() {
    let error = CredentialAccessError::new(
        CredentialReference::new(BRAVE_SEARCH_CREDENTIAL_REFERENCE),
        CredentialAccessFailure::Unmapped,
    );
    let correlation = dispatch_correlation();

    let diagnostic = capture_credential_failure(&error, &correlation);

    assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(CREDENTIAL_FAILURE_CLASSIFICATION));
    assert!(!diagnostic.contains(SYNTHETIC_KEY));
}

/// credential-resolution telemetry cannot inherit credential
/// bytes from an entered caller span.
#[test]
fn credential_failure_diagnostic_ignores_credential_bearing_caller_span() {
    let error = CredentialAccessError::new(
        CredentialReference::new(BRAVE_SEARCH_CREDENTIAL_REFERENCE),
        CredentialAccessFailure::Unmapped,
    );
    let correlation = dispatch_correlation();

    let diagnostic =
        capture_credential_failure_in_credential_span(&error, &correlation, SYNTHETIC_KEY);

    assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(CREDENTIAL_FAILURE_CLASSIFICATION));
    assert!(!diagnostic.contains(SYNTHETIC_KEY));
}

/// unusable-credential telemetry cannot retain the resolved
/// credential bytes.
#[test]
fn unusable_credential_value_diagnostic_preserves_safe_classification() {
    let correlation = dispatch_correlation();
    let credential = CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec());

    let (diagnostic, result) = capture_credential_value_failure(&correlation, &credential);

    result.expect("safe credential value diagnostic is emitted");
    assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(CREDENTIAL_VALUE_FAILURE_CLASSIFICATION));
    assert!(!diagnostic.contains(SYNTHETIC_KEY));
}

/// unusable-credential telemetry cannot inherit resolved
/// credential bytes from an entered caller span.
#[test]
fn unusable_credential_value_diagnostic_ignores_credential_bearing_caller_span() {
    let correlation = dispatch_correlation();
    let credential = CredentialValue::new(LEADING_HEADER_WHITESPACE_KEY.as_bytes().to_vec());

    let (diagnostic, result) =
        capture_credential_value_failure_in_credential_span(&correlation, &credential);

    result.expect("safe credential value diagnostic is emitted outside the caller span");
    assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(CREDENTIAL_VALUE_FAILURE_CLASSIFICATION));
    assert!(!diagnostic.contains(LEADING_HEADER_WHITESPACE_KEY));
}

/// an unusable one-byte credential suppresses colliding telemetry
/// and cannot form a colliding public bound result.
#[tokio::test]
async fn unusable_short_credential_value_diagnostic_is_suppressed() {
    let correlation = dispatch_correlation();
    let credential = CredentialValue::new(SHORT_DIAGNOSTIC_COLLISION_KEY.as_bytes().to_vec());

    let (diagnostic, result) = capture_credential_value_failure(&correlation, &credential);
    let report_error = result.expect_err("credential collision suppresses the diagnostic");
    let (failed, searches, _rendered) =
        execute_formatted_raw_credential_through_service(SHORT_DIAGNOSTIC_COLLISION_KEY.as_bytes())
            .await;

    assert!(!diagnostic.contains(SHORT_DIAGNOSTIC_COLLISION_KEY));
    assert!(!format!("{report_error:?}").contains(SHORT_DIAGNOSTIC_COLLISION_KEY));
    assert!(
        !report_error
            .to_string()
            .contains(SHORT_DIAGNOSTIC_COLLISION_KEY)
    );
    assert!(failed);
    assert_eq!(searches, 0);
}

/// transport-failure telemetry cannot retain the request
/// credential.
#[test]
fn transport_failure_diagnostic_preserves_safe_classification() {
    let correlation = dispatch_correlation();
    let failure = WebSearchTransportFailure::RequestFailed;
    let credential = CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec());

    let (diagnostic, result) = capture_transport_failure(&failure, &correlation, &credential);

    result.expect("safe transport diagnostic is emitted");
    assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(TRANSPORT_FAILURE_CLASSIFICATION));
    assert!(!diagnostic.contains(SYNTHETIC_KEY));
}

/// an incomplete provider-rejection body emits its retained safe
/// failure class before the definitive rejection is coarsened.
#[test]
fn provider_rejection_body_failure_reports_safe_classification() {
    let correlation = dispatch_correlation();
    let credential = CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec());

    let (diagnostic, result) = capture_response_body_failure(
        WebSearchTransportFailureClass::DispatchUnknown,
        &correlation,
        &credential,
    );

    result.expect("safe provider-response body diagnostic is emitted");
    assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(RESPONSE_BODY_FAILURE_CLASSIFICATION));
    assert!(!diagnostic.contains(SYNTHETIC_KEY));
}

/// post-response sanitization reports a credential-safe closed
/// discriminant before its typed error becomes invalid-response evidence.
#[test]
fn response_sanitization_failure_reports_safe_classification() {
    let correlation = dispatch_correlation();
    let credential = CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec());

    let (diagnostic, result) = capture_response_sanitization_failure(&correlation, &credential);

    result.expect("safe response-sanitization diagnostic is emitted");
    assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
    assert!(diagnostic.contains(RESPONSE_SANITIZATION_FAILURE_CLASSIFICATION));
    assert!(!diagnostic.contains(SYNTHETIC_KEY));
}

/// a case-normalized credential collision in the controlled
/// response-sanitization event suppresses telemetry and stays opaque.
#[test]
fn response_sanitization_failure_omits_case_normalized_credential_collision() {
    let correlation = dispatch_correlation();
    let credential = CredentialValue::new(
        RESPONSE_SANITIZATION_CASE_NORMALIZED_COLLISION_KEY
            .as_bytes()
            .to_vec(),
    );

    let (diagnostic, result) = capture_response_sanitization_failure(&correlation, &credential);

    let error = result.expect_err("case-normalized credential suppresses the event");
    assert!(!unicode_case_insensitive_contains(
        &diagnostic,
        RESPONSE_SANITIZATION_CASE_NORMALIZED_COLLISION_KEY,
    ));
    assert!(!unicode_case_insensitive_contains(
        &format!("{error:?}"),
        RESPONSE_SANITIZATION_CASE_NORMALIZED_COLLISION_KEY,
    ));
    assert!(!unicode_case_insensitive_contains(
        &error.to_string(),
        RESPONSE_SANITIZATION_CASE_NORMALIZED_COLLISION_KEY,
    ));
}

/// compact formatter timestamps and ANSI metadata are accounted
/// for before a post-credential transport event can be emitted.
#[test]
fn web_search_transport_event_omits_timestamp_credential_collision() {
    let correlation = dispatch_correlation();
    let failure = WebSearchTransportFailure::RequestFailed;
    let credential = CredentialValue::new(TIMESTAMP_COLLISION_KEY.as_bytes().to_vec());

    let (diagnostic, result) = capture_transport_failure(&failure, &correlation, &credential);

    let error = result.expect_err("timestamp-shaped credential suppresses the event");
    assert!(!diagnostic.contains(TIMESTAMP_COLLISION_KEY));
    assert!(!format!("{error:?}").contains(TIMESTAMP_COLLISION_KEY));
    assert!(!error.to_string().contains(TIMESTAMP_COLLISION_KEY));
}

/// a credential spanning compact formatter metadata and event
/// text suppresses the complete daemon-shaped event.
#[test]
fn web_search_transport_event_omits_formatter_boundary_collision() {
    let correlation = dispatch_correlation();
    let failure = WebSearchTransportFailure::RequestFailed;
    let credential =
        CredentialValue::new(FORMATTER_EVENT_BOUNDARY_COLLISION_KEY.as_bytes().to_vec());

    let (diagnostic, result) = capture_transport_failure(&failure, &correlation, &credential);

    let error = result.expect_err("formatter-boundary credential suppresses the event");
    assert!(!diagnostic.contains(FORMATTER_EVENT_BOUNDARY_COLLISION_KEY));
    assert!(diagnostic.is_empty());
    assert!(!format!("{error:?}").contains(FORMATTER_EVENT_BOUNDARY_COLLISION_KEY));
    assert!(
        !error
            .to_string()
            .contains(FORMATTER_EVENT_BOUNDARY_COLLISION_KEY)
    );
}
