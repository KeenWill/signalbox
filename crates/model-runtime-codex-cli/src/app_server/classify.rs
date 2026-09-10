use super::frame::{CodexErrorInfo, KnownError, RpcError, TurnStatus};
use signalbox_model_runtime::ProviderErrorKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureClass {
    Provider(ProviderErrorKind),
    PolicyRefusal(signalbox_model_runtime::RefusalReason),
}

pub(crate) fn classify(info: Option<&CodexErrorInfo>) -> FailureClass {
    let status_kind = match info.and_then(CodexErrorInfo::http_status) {
        Some(401) => Some(ProviderErrorKind::CredentialRejected),
        Some(429) => Some(ProviderErrorKind::RateLimited),
        Some(500) => Some(ProviderErrorKind::ProviderInternal),
        Some(503 | 529) => Some(ProviderErrorKind::Overloaded),
        _ => None,
    };
    if let Some(kind) = status_kind {
        return FailureClass::Provider(kind);
    }
    let Some(CodexErrorInfo::Known(info)) = info else {
        return FailureClass::Provider(ProviderErrorKind::Unrecognized);
    };
    FailureClass::Provider(match info {
        KnownError::ContextWindowExceeded => ProviderErrorKind::RequestTooLarge,
        KnownError::UsageLimitExceeded => ProviderErrorKind::QuotaExhausted,
        KnownError::RateLimitExceeded => ProviderErrorKind::RateLimited,
        KnownError::ServerOverloaded => ProviderErrorKind::Overloaded,
        KnownError::Unauthorized => ProviderErrorKind::CredentialRejected,
        KnownError::BadRequest => ProviderErrorKind::InvalidRequest,
        KnownError::InternalServerError => ProviderErrorKind::ProviderInternal,
        KnownError::CyberPolicy => {
            return FailureClass::PolicyRefusal(
                signalbox_model_runtime::RefusalReason::CyberPolicy,
            );
        }
        KnownError::MisalignmentPolicyViolation => {
            return FailureClass::PolicyRefusal(
                signalbox_model_runtime::RefusalReason::Misalignment,
            );
        }
        KnownError::SessionBudgetExceeded
        | KnownError::HttpConnectionFailed { .. }
        | KnownError::ResponseStreamConnectionFailed { .. }
        | KnownError::ThreadRollbackFailed
        | KnownError::SandboxError
        | KnownError::ResponseStreamDisconnected { .. }
        | KnownError::ResponseTooManyFailedAttempts { .. }
        | KnownError::ActiveTurnNotSteerable { .. }
        | KnownError::Other => ProviderErrorKind::Unrecognized,
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TurnActivity {
    pub(crate) response_content_observed: bool,
    pub(crate) assistant_output_observed: bool,
    pub(crate) retry_observed: bool,
}

impl TurnActivity {
    pub(crate) fn proves_non_acceptance(
        self,
        status: TurnStatus,
        info: Option<&CodexErrorInfo>,
    ) -> bool {
        status == TurnStatus::Failed
            && !self.assistant_output_observed
            && !self.retry_observed
            && matches!(
                info,
                Some(CodexErrorInfo::Known(
                    KnownError::ContextWindowExceeded
                        | KnownError::UsageLimitExceeded
                        | KnownError::RateLimitExceeded
                        | KnownError::ServerOverloaded
                        | KnownError::HttpConnectionFailed { .. }
                        | KnownError::ResponseStreamConnectionFailed { .. }
                        | KnownError::InternalServerError
                        | KnownError::BadRequest
                ))
            )
    }
}

pub(crate) fn input_too_large(method: &str, error: &RpcError) -> bool {
    method == "turn/start"
        && error.code == -32602
        && error
            .data
            .as_ref()
            .and_then(|data| data.get("input_error_code"))
            .and_then(serde_json::Value::as_str)
            == Some("input_too_large")
}
