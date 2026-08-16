//! Conservative classification of Codex CLI failure messages.

use std::time::Duration;

use signalbox_model_runtime::ProviderErrorKind;

/// Classifies the CLI's rendered error surface.
///
/// Codex exec exposes a message but no stable HTTP status or native code in
/// its JSONL failure event. Credential rejection has first precedence; every
/// other mapping requires an explicit phrase and unknown text fails closed.
pub(crate) fn classify_error(message: &str) -> ProviderErrorKind {
    let normalized = message.to_ascii_lowercase();
    match () {
        _ if contains_any(
            &normalized,
            &[
                "not logged in",
                "unauthorized",
                "authentication failed",
                "invalid api key",
                "invalid_api_key",
                "refresh_token_expired",
                "refresh_token_reused",
                "refresh_token_invalidated",
                "refresh token rejected",
                "invalid refresh token",
                "credential rejected",
                "authentication_error",
            ],
        ) =>
        {
            ProviderErrorKind::CredentialRejected
        }
        _ if contains_any(
            &normalized,
            &["permission denied", "forbidden", "permission_denied"],
        ) =>
        {
            ProviderErrorKind::PermissionDenied
        }
        _ if contains_any(
            &normalized,
            &[
                "model not found",
                "model_not_found",
                "unknown model",
                "target not found",
            ],
        ) =>
        {
            ProviderErrorKind::TargetNotFound
        }
        _ if contains_any(
            &normalized,
            &[
                "request too large",
                "context length exceeded",
                "context_length_exceeded",
            ],
        ) =>
        {
            ProviderErrorKind::RequestTooLarge
        }
        _ if contains_any(
            &normalized,
            &[
                "quota exhausted",
                "insufficient quota",
                "insufficient_quota",
                "usage limit reached",
                "hit your usage limit",
                "usagelimitreached",
            ],
        ) =>
        {
            ProviderErrorKind::QuotaExhausted
        }
        _ if contains_any(
            &normalized,
            &["rate limit", "rate_limit", "too many requests"],
        ) =>
        {
            ProviderErrorKind::RateLimited
        }
        _ if contains_any(&normalized, &["overloaded", "at capacity"]) => {
            ProviderErrorKind::Overloaded
        }
        _ if contains_any(
            &normalized,
            &["internal server error", "provider internal", "server_error"],
        ) =>
        {
            ProviderErrorKind::ProviderInternal
        }
        _ if contains_any(
            &normalized,
            &["invalid request", "bad request", "invalid_request_error"],
        ) =>
        {
            ProviderErrorKind::InvalidRequest
        }
        _ => ProviderErrorKind::Unrecognized,
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// Extracts the CLI's bounded rendered retry hint without retaining the text.
pub(crate) fn retry_after(message: &str) -> Option<Duration> {
    let normalized = message.to_ascii_lowercase();
    let marker = ["retry after ", "try again in "]
        .into_iter()
        .find_map(|marker| normalized.find(marker).map(|index| (marker, index)))?;
    let remainder = &normalized[marker.1 + marker.0.len()..];
    let mut parts = remainder.split_whitespace();
    let rendered_amount = parts.next()?;
    if rendered_amount.starts_with('-') || rendered_amount.starts_with('+') {
        return None;
    }
    let amount = rendered_amount.trim_matches(|character: char| !character.is_ascii_digit());
    let amount = amount.parse::<u64>().ok()?;
    let unit = parts
        .next()?
        .trim_matches(|character: char| !character.is_ascii_alphabetic());
    match unit {
        "second" | "seconds" => Some(Duration::from_secs(amount)),
        "minute" | "minutes" => amount.checked_mul(60).map(Duration::from_secs),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use signalbox_model_runtime::ProviderErrorKind;

    use super::{classify_error, retry_after};

    #[test]
    fn every_provider_error_kind_has_an_explicit_cli_message_mapping() {
        assert_eq!(
            classify_error("authentication failed: refusal"),
            ProviderErrorKind::CredentialRejected
        );
        assert_eq!(
            classify_error("permission denied"),
            ProviderErrorKind::PermissionDenied
        );
        assert_eq!(
            classify_error("invalid request"),
            ProviderErrorKind::InvalidRequest
        );
        assert_eq!(
            classify_error("model not found"),
            ProviderErrorKind::TargetNotFound
        );
        assert_eq!(
            classify_error("request too large"),
            ProviderErrorKind::RequestTooLarge
        );
        assert_eq!(
            classify_error("rate limit exceeded"),
            ProviderErrorKind::RateLimited
        );
        assert_eq!(
            classify_error("insufficient_quota"),
            ProviderErrorKind::QuotaExhausted
        );
        assert_eq!(
            classify_error("You've hit your usage limit."),
            ProviderErrorKind::QuotaExhausted
        );
        assert_eq!(
            classify_error("provider overloaded"),
            ProviderErrorKind::Overloaded
        );
        assert_eq!(
            classify_error("internal server error"),
            ProviderErrorKind::ProviderInternal
        );
        assert_eq!(
            classify_error("some future failure"),
            ProviderErrorKind::Unrecognized
        );
    }

    /// A CLI failure rendered solely with the native underscore-form token —
    /// no `400 Bad Request` prose wrapper — keeps its typed kind rather than
    /// degrading to `Unrecognized`, preserving the shared failure taxonomy.
    #[test]
    fn underscore_form_native_tokens_keep_their_typed_kind() {
        assert_eq!(
            classify_error("invalid_request_error"),
            ProviderErrorKind::InvalidRequest
        );
        assert_eq!(
            classify_error("authentication_error"),
            ProviderErrorKind::CredentialRejected
        );
        assert_eq!(
            classify_error("permission_denied"),
            ProviderErrorKind::PermissionDenied
        );
    }

    #[test]
    fn credential_rejection_precedes_refusal_shaped_text() {
        assert_eq!(
            classify_error("refusal followed by authentication failed"),
            ProviderErrorKind::CredentialRejected
        );
    }

    #[test]
    fn permanent_refresh_failure_is_a_credential_rejection() {
        assert_eq!(
            classify_error("refresh_token_invalidated"),
            ProviderErrorKind::CredentialRejected
        );
    }

    #[test]
    fn transient_refresh_failure_is_not_a_credential_rejection() {
        assert_eq!(
            classify_error("refresh token request timed out"),
            ProviderErrorKind::Unrecognized
        );
    }

    #[test]
    fn rendered_retry_hint_is_typed_and_bounded_to_known_units() {
        assert_eq!(
            retry_after("rate limit exceeded; retry after 17 seconds"),
            Some(std::time::Duration::from_secs(17))
        );
        assert_eq!(
            retry_after("at capacity, try again in 2 minutes"),
            Some(std::time::Duration::from_secs(120))
        );
        assert_eq!(retry_after("retry later"), None);
        assert_eq!(retry_after("retry after -1 seconds"), None);
        assert_eq!(retry_after("retry after +1 seconds"), None);
    }

    #[test]
    fn specific_native_errors_precede_a_generic_bad_request_wrapper() {
        assert_eq!(
            classify_error("400 Bad Request: context_length_exceeded"),
            ProviderErrorKind::RequestTooLarge
        );
        assert_eq!(
            classify_error("400 Bad Request: model not found"),
            ProviderErrorKind::TargetNotFound
        );
        assert_eq!(
            classify_error(
                r#"unexpected status 404 Not Found: {"error":{"code":"model_not_found"}}"#
            ),
            ProviderErrorKind::TargetNotFound
        );
        assert_eq!(
            classify_error("400 Bad Request: insufficient_quota"),
            ProviderErrorKind::QuotaExhausted
        );
        assert_eq!(
            classify_error("400 Bad Request: rate_limit"),
            ProviderErrorKind::RateLimited
        );
        assert_eq!(
            classify_error("400 Bad Request: provider overloaded"),
            ProviderErrorKind::Overloaded
        );
        assert_eq!(
            classify_error("400 Bad Request: server_error"),
            ProviderErrorKind::ProviderInternal
        );
    }
}
