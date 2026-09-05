//! Classification of machine-readable Claude Code HTTP status values.

use signalbox_model_runtime::ProviderErrorKind;

pub(crate) fn classify_status(status: Option<u16>) -> ProviderErrorKind {
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
