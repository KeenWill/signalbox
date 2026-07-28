//! Conservative classification of Claude Code failure messages.

use signalbox_model_runtime::ProviderErrorKind;

pub(crate) fn classify_error(subtype: &str, message: &str) -> ProviderErrorKind {
    let normalized = format!("{subtype} {message}").to_ascii_lowercase();
    match () {
        _ if contains_any(
            &normalized,
            &[
                "not logged in",
                "unauthorized",
                "authentication failed",
                "invalid api key",
                "invalid_api_key",
                "oauth token",
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
            &["model not found", "model_not_found", "unknown model"],
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
                "error_max_budget_usd",
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
