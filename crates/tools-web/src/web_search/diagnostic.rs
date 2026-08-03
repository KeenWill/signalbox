use std::{error::Error, fmt};

use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_model_runtime::CredentialValue;

use super::redaction::*;

/// Opaque request-scoped diagnostic proven not to contain its credential.
#[derive(Clone, Eq, PartialEq)]
pub struct WebSearchCredentialDiagnostic {
    pub(super) rendered: String,
    pub(super) failure_class: WebSearchCredentialDiagnosticClass,
    pub(super) transport_failure_class: Option<WebSearchTransportFailureClass>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WebSearchCredentialDiagnosticClass {
    CallerOrHubBug,
    InfrastructureCommitAmbiguous,
}

impl fmt::Debug for WebSearchCredentialDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.rendered)
    }
}

impl fmt::Display for WebSearchCredentialDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.rendered)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WebSearchTransportFailureClass {
    InvalidCredential,
    CredentialDiagnosticCollision,
    RequestFailed,
    ProviderRejected,
    InvalidResponse,
    ResponseTooLarge,
    DispatchUnknown,
}

pub(super) fn safe_collision_diagnostic(credential: &str) -> String {
    const DIAGNOSTIC: &str = "web search credential diagnostic suppressed";
    const REDACTION: &str = "[redacted]";
    if credential.is_empty() {
        return String::from(DIAGNOSTIC);
    }
    let redacted = DIAGNOSTIC.replace(credential, REDACTION);
    if !text_contains_credential_variant(&redacted, credential) {
        return redacted;
    }
    if credential == "!" {
        String::from("?")
    } else {
        String::from("!")
    }
}

pub(super) fn credential_safe_executor_error(
    error: WebSearchExecutorError,
    credential: &CredentialValue,
) -> WebSearchExecutorError {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let failure_class = executor_error_diagnostic_class(&error);
    let transport_failure_class = executor_error_transport_failure_class(&error);
    if credential_text.is_empty()
        || text_contains_credential_variant(&format!("{error:?}"), credential_text)
        || text_contains_credential_variant(&error.to_string(), credential_text)
    {
        WebSearchExecutorError::CredentialDiagnosticCollision(WebSearchCredentialDiagnostic {
            rendered: safe_collision_diagnostic(credential_text),
            failure_class,
            transport_failure_class,
        })
    } else {
        error
    }
}

pub(super) fn executor_error_transport_failure_class(
    error: &WebSearchExecutorError,
) -> Option<WebSearchTransportFailureClass> {
    match error {
        WebSearchExecutorError::CredentialDiagnosticCollision(diagnostic) => {
            diagnostic.transport_failure_class
        }
        WebSearchExecutorError::ArgumentValidationDrift
        | WebSearchExecutorError::EvidenceEncoding
        | WebSearchExecutorError::DispatchUnknown => None,
    }
}

pub(super) fn executor_error_diagnostic_class(
    error: &WebSearchExecutorError,
) -> WebSearchCredentialDiagnosticClass {
    match error {
        WebSearchExecutorError::ArgumentValidationDrift
        | WebSearchExecutorError::EvidenceEncoding => {
            WebSearchCredentialDiagnosticClass::CallerOrHubBug
        }
        WebSearchExecutorError::DispatchUnknown => {
            WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous
        }
        WebSearchExecutorError::CredentialDiagnosticCollision(diagnostic) => {
            diagnostic.failure_class
        }
    }
}

/// A checked catalog/executor assumption failed inside `web_search`.
#[derive(Clone, Eq, PartialEq)]
pub enum WebSearchExecutorError {
    /// Executor argument decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    /// Sanitized result or error evidence could not be encoded.
    EvidenceEncoding,
    /// Physical dispatch began without a complete bounded outcome.
    DispatchUnknown,
    /// A diagnostic collided with its request credential.
    CredentialDiagnosticCollision(WebSearchCredentialDiagnostic),
}

impl fmt::Debug for WebSearchExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArgumentValidationDrift => formatter.write_str("ArgumentValidationDrift"),
            Self::EvidenceEncoding => formatter.write_str("EvidenceEncoding"),
            Self::DispatchUnknown => formatter.write_str("DispatchUnknown"),
            Self::CredentialDiagnosticCollision(diagnostic) => {
                formatter.write_str(&diagnostic.rendered)
            }
        }
    }
}

impl fmt::Display for WebSearchExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArgumentValidationDrift => {
                formatter.write_str("web_search argument validation drifted")
            }
            Self::EvidenceEncoding => formatter.write_str("web_search evidence encoding failed"),
            Self::DispatchUnknown => formatter.write_str("web_search dispatch outcome is unknown"),
            Self::CredentialDiagnosticCollision(diagnostic) => {
                formatter.write_str(&diagnostic.rendered)
            }
        }
    }
}

impl Error for WebSearchExecutorError {}

impl ClassifyOperatorFailure for WebSearchExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::ArgumentValidationDrift | Self::EvidenceEncoding => {
                OperatorFailureClass::CallerOrHubBug
            }
            Self::DispatchUnknown => OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
            Self::CredentialDiagnosticCollision(diagnostic) => match diagnostic.failure_class {
                WebSearchCredentialDiagnosticClass::CallerOrHubBug => {
                    OperatorFailureClass::CallerOrHubBug
                }
                WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous => {
                    OperatorFailureClass::Infrastructure {
                        commit_ambiguous: true,
                    }
                }
            },
        }
    }
}
