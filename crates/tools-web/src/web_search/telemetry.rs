use signalbox_domain::ToolAttemptDispatchCorrelation;
use signalbox_model_runtime::{CredentialAccessError, CredentialValue};

use super::{diagnostic::*, redaction::*, text_decoding::*, transport_failure::*};

pub(super) fn report_credential_access_failure(
    error: &CredentialAccessError,
    correlation: &ToolAttemptDispatchCorrelation,
) {
    tracing::warn!(
        target: "signalbox_tools_basic_web_search",
        parent: None,
        failure = ?error.failure,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "web search credential resolution failed"
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CredentialValueFailure {
    Unusable,
}

pub(super) fn report_credential_value_failure(
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> Result<(), WebSearchExecutorError> {
    if credential.expose_bytes().len() > MAX_CREDENTIAL_BYTES {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: String::new(),
                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                transport_failure_class: None,
            },
        ));
    }
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let controlled_event = format!(
        "WARN signalbox_tools_basic_web_search: web search credential value was unusable failure={:?} session_id={} turn_id={}",
        CredentialValueFailure::Unusable,
        correlation.session().as_uuid(),
        correlation.turn().as_uuid()
    );
    if credential_text.is_empty()
        || compact_formatter_framing_may_contain(credential_text, &controlled_event)
        || text_contains_credential_variant(&controlled_event, credential_text)
    {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                transport_failure_class: None,
            },
        ));
    }
    tracing::warn!(
        target: "signalbox_tools_basic_web_search",
        parent: None,
        failure = ?CredentialValueFailure::Unusable,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "web search credential value was unusable"
    );
    Ok(())
}

pub(super) fn report_transport_failure(
    failure: &WebSearchTransportFailure,
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> Result<(), WebSearchExecutorError> {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let controlled_event = format!(
        "WARN signalbox_tools_basic_web_search: web search transport failed failure={:?} session_id={} turn_id={}",
        failure.class(),
        correlation.session().as_uuid(),
        correlation.turn().as_uuid()
    );
    if credential_text.is_empty()
        || compact_formatter_framing_may_contain(credential_text, &controlled_event)
        || text_contains_credential_variant(&controlled_event, credential_text)
    {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class: transport_failure_diagnostic_class(failure),
                transport_failure_class: transport_failure_source_class(failure),
            },
        ));
    }
    tracing::event!(
        target: "signalbox_tools_basic_web_search",
        parent: None,
        tracing::Level::WARN,
        failure = ?failure.class(),
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "web search transport failed"
    );
    Ok(())
}

pub(super) fn report_response_body_failure(
    failure_class: WebSearchTransportFailureClass,
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> Result<(), WebSearchExecutorError> {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let controlled_event = format!(
        "WARN signalbox_tools_basic_web_search: web search provider response body failed failure={failure_class:?} session_id={} turn_id={}",
        correlation.session().as_uuid(),
        correlation.turn().as_uuid()
    );
    if credential_text.is_empty()
        || compact_formatter_framing_may_contain(credential_text, &controlled_event)
        || text_contains_credential_variant(&controlled_event, credential_text)
    {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                transport_failure_class: Some(failure_class),
            },
        ));
    }
    tracing::event!(
        target: "signalbox_tools_basic_web_search",
        parent: None,
        tracing::Level::WARN,
        failure = ?failure_class,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "web search provider response body failed"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResponseSanitizationFailure {
    EvidenceEncoding,
}

pub(super) fn report_response_sanitization_failure(
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> Result<(), WebSearchExecutorError> {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let controlled_event = format!(
        "WARN signalbox_tools_basic_web_search: web search response sanitization failed failure={:?} session_id={} turn_id={}",
        ResponseSanitizationFailure::EvidenceEncoding,
        correlation.session().as_uuid(),
        correlation.turn().as_uuid()
    );
    if credential_text.is_empty()
        || compact_formatter_framing_may_contain(credential_text, &controlled_event)
        || text_contains_credential_variant(&controlled_event, credential_text)
    {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                transport_failure_class: None,
            },
        ));
    }
    tracing::event!(
        target: "signalbox_tools_basic_web_search",
        parent: None,
        tracing::Level::WARN,
        failure = ?ResponseSanitizationFailure::EvidenceEncoding,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "web search response sanitization failed"
    );
    Ok(())
}

pub(super) fn compact_formatter_framing_may_contain(
    credential: &str,
    controlled_event: &str,
) -> bool {
    formatter_variant_may_span_event(credential, controlled_event)
        || decoded_credential_variants(credential).is_none_or(|variants| {
            variants
                .iter()
                .any(|variant| formatter_variant_may_span_event(variant, controlled_event))
        })
}

pub(super) fn formatter_variant_may_span_event(credential: &str, controlled_event: &str) -> bool {
    const DYNAMIC_METADATA_CHARACTERS: &str = "0123456789-:+.TZ \r\n";
    if credential.contains('\u{1b}')
        || credential
            .chars()
            .all(|character| DYNAMIC_METADATA_CHARACTERS.contains(character))
    {
        return true;
    }
    let normalized_credential = unicode_case_folded_nfd(credential);
    let normalized_event = unicode_case_folded_nfd(controlled_event);
    let dynamic_prefix_end = credential
        .char_indices()
        .find(|(_, character)| !DYNAMIC_METADATA_CHARACTERS.contains(*character))
        .map_or(credential.len(), |(index, _)| index);
    let dynamic_suffix_start = credential
        .char_indices()
        .rev()
        .find(|(_, character)| !DYNAMIC_METADATA_CHARACTERS.contains(*character))
        .map_or(0, |(index, character)| index + character.len_utf8());
    credential.char_indices().skip(1).any(|(split, _)| {
        let trailing_length = credential.len() - split;
        (split <= dynamic_prefix_end
            && normalized_event.starts_with(&normalized_credential[split..]))
            || (split >= dynamic_suffix_start
                && normalized_event.ends_with(
                    &normalized_credential[..normalized_credential.len() - trailing_length],
                ))
    })
}
