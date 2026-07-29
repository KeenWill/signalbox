//! Conservative classification of Claude Code failure messages.

use signalbox_model_runtime::ProviderErrorKind;

pub(crate) fn classify_error(
    status: Option<u16>,
    subtype: &str,
    message: &str,
) -> ProviderErrorKind {
    let normalized = format!("{subtype} {message}").to_ascii_lowercase();
    let text_kind = match () {
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
    };
    if status == Some(401) {
        return ProviderErrorKind::CredentialRejected;
    }
    if text_kind != ProviderErrorKind::Unrecognized {
        return text_kind;
    }
    match status {
        Some(400) => ProviderErrorKind::InvalidRequest,
        Some(401) => ProviderErrorKind::CredentialRejected,
        Some(403) => ProviderErrorKind::PermissionDenied,
        Some(404) => ProviderErrorKind::TargetNotFound,
        Some(413) => ProviderErrorKind::RequestTooLarge,
        Some(429) => ProviderErrorKind::RateLimited,
        Some(500) => ProviderErrorKind::ProviderInternal,
        Some(529) => ProviderErrorKind::Overloaded,
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
    fn definitive_status_classifies_an_otherwise_generic_error() {
        assert_eq!(
            classify_error(Some(401), "error_during_execution", ""),
            ProviderErrorKind::CredentialRejected
        );
        assert_eq!(
            classify_error(Some(429), "error_during_execution", ""),
            ProviderErrorKind::RateLimited
        );
        assert_eq!(
            classify_error(Some(529), "error_during_execution", ""),
            ProviderErrorKind::Overloaded
        );
    }
}
