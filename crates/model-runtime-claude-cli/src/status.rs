//! Classification from Claude Code terminal subtype and HTTP status.

use signalbox_model_runtime::ProviderErrorKind;

use crate::wire::ResultSubtype;

pub(crate) fn classify_error(status: Option<u16>, subtype: &ResultSubtype) -> ProviderErrorKind {
    match status {
        Some(400) => ProviderErrorKind::InvalidRequest,
        Some(401) => ProviderErrorKind::CredentialRejected,
        Some(403) => ProviderErrorKind::PermissionDenied,
        Some(404) => ProviderErrorKind::TargetNotFound,
        Some(413) => ProviderErrorKind::RequestTooLarge,
        Some(429) => ProviderErrorKind::RateLimited,
        Some(500) => ProviderErrorKind::ProviderInternal,
        Some(503 | 529) => ProviderErrorKind::Overloaded,
        _ => match subtype {
            ResultSubtype::ErrorMaxBudgetUsd => ProviderErrorKind::QuotaExhausted,
            ResultSubtype::Success
            | ResultSubtype::ErrorDuringExecution
            | ResultSubtype::ErrorMaxTurns
            | ResultSubtype::ErrorMaxStructuredOutputRetries
            | ResultSubtype::Unknown(_) => ProviderErrorKind::Unrecognized,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_classifies_generic_terminal_subtypes() {
        for (status, expected) in [
            (400, ProviderErrorKind::InvalidRequest),
            (401, ProviderErrorKind::CredentialRejected),
            (403, ProviderErrorKind::PermissionDenied),
            (404, ProviderErrorKind::TargetNotFound),
            (413, ProviderErrorKind::RequestTooLarge),
            (429, ProviderErrorKind::RateLimited),
            (500, ProviderErrorKind::ProviderInternal),
            (503, ProviderErrorKind::Overloaded),
            (529, ProviderErrorKind::Overloaded),
        ] {
            assert_eq!(
                classify_error(Some(status), &ResultSubtype::Success),
                expected
            );
        }
    }

    #[test]
    fn http_status_precedes_budget_subtype() {
        assert_eq!(
            classify_error(Some(429), &ResultSubtype::ErrorMaxBudgetUsd),
            ProviderErrorKind::RateLimited
        );
    }

    #[test]
    fn a_native_budget_failure_is_quota_exhaustion() {
        assert_eq!(
            classify_error(None, &ResultSubtype::ErrorMaxBudgetUsd),
            ProviderErrorKind::QuotaExhausted
        );
    }
}
