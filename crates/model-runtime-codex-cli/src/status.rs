//! Conservative classification of Codex CLI failure messages.

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
            ],
        ) =>
        {
            ProviderErrorKind::CredentialRejected
        }
        _ if contains_any(&normalized, &["permission denied", "forbidden"]) => {
            ProviderErrorKind::PermissionDenied
        }
        _ if contains_any(&normalized, &["invalid request", "bad request"]) => {
            ProviderErrorKind::InvalidRequest
        }
        _ if contains_any(
            &normalized,
            &["model not found", "unknown model", "target not found"],
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
                "usage limit reached",
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
        _ => ProviderErrorKind::Unrecognized,
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use signalbox_model_runtime::ProviderErrorKind;

    use super::classify_error;

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
            classify_error("quota exhausted"),
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
}
