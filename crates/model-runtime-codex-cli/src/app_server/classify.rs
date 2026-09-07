use super::frame::{CodexErrorInfo, KnownError, RpcError, TurnStatus};
use signalbox_model_runtime::ProviderErrorKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureClass {
    Provider(ProviderErrorKind),
    PolicyRefusal,
}

pub(crate) fn classify(info: Option<&CodexErrorInfo>) -> FailureClass {
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
        KnownError::CyberPolicy | KnownError::MisalignmentPolicyViolation => {
            return FailureClass::PolicyRefusal;
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
