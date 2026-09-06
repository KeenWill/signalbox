use std::fmt;

use signalbox_model_runtime::CredentialValue;

use super::{diagnostic::*, redaction::*, result::*};

#[derive(signalbox_derive::OperatorError)]
/// Sanitized classification of one physical provider exchange.
pub enum WebSearchTransportFailure {
    #[error("invalid web search credential")]
    /// Client-side credential bytes could not form the provider header.
    InvalidCredential,
    #[error("{}", & field_0.rendered)]
    /// A request-scoped credential collided with otherwise-safe diagnostics.
    CredentialDiagnosticCollision(WebSearchCredentialDiagnostic),
    #[error("web search request failed before dispatch")]
    /// Client setup or connection failed before dispatch.
    RequestFailed,
    #[error("web search provider rejected the request")]
    /// A complete status and complete bounded provider error body were received.
    ProviderRejected(#[source] WebSearchProviderError),
    #[error("web search provider returned an invalid response")]
    /// A complete success body did not match the provider contract.
    InvalidResponse,
    #[error("web search provider response exceeded the byte cap")]
    /// The provider body exceeded the fixed exchange cap.
    ResponseTooLarge,
    #[error("web search request outcome is unknown")]
    /// Dispatch began without a complete bounded outcome.
    DispatchUnknown,
}

impl fmt::Debug for WebSearchTransportFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCredential => formatter.write_str("InvalidCredential"),
            Self::CredentialDiagnosticCollision(diagnostic) => {
                formatter.write_str(&diagnostic.rendered)
            }
            Self::RequestFailed => formatter.write_str("RequestFailed"),
            Self::ProviderRejected(_) => formatter.write_str("ProviderRejected"),
            Self::InvalidResponse => formatter.write_str("InvalidResponse"),
            Self::ResponseTooLarge => formatter.write_str("ResponseTooLarge"),
            Self::DispatchUnknown => formatter.write_str("DispatchUnknown"),
        }
    }
}

impl WebSearchTransportFailure {
    pub(super) const fn class(&self) -> WebSearchTransportFailureClass {
        match self {
            Self::InvalidCredential => WebSearchTransportFailureClass::InvalidCredential,
            Self::CredentialDiagnosticCollision(_) => {
                WebSearchTransportFailureClass::CredentialDiagnosticCollision
            }
            Self::RequestFailed => WebSearchTransportFailureClass::RequestFailed,
            Self::ProviderRejected(_) => WebSearchTransportFailureClass::ProviderRejected,
            Self::InvalidResponse => WebSearchTransportFailureClass::InvalidResponse,
            Self::ResponseTooLarge => WebSearchTransportFailureClass::ResponseTooLarge,
            Self::DispatchUnknown => WebSearchTransportFailureClass::DispatchUnknown,
        }
    }

    pub(super) const fn response_body_failure_class(
        &self,
    ) -> Option<WebSearchTransportFailureClass> {
        match self {
            Self::ProviderRejected(error) => error.body_failure_class,
            Self::InvalidCredential
            | Self::CredentialDiagnosticCollision(_)
            | Self::RequestFailed
            | Self::InvalidResponse
            | Self::ResponseTooLarge
            | Self::DispatchUnknown => None,
        }
    }
}

/// Credential-sanitized result of one injected transport request.
pub struct WebSearchTransportOutcome {
    pub(super) result: Result<WebSearchResponse, WebSearchTransportFailure>,
}

impl WebSearchTransportOutcome {
    /// Builds one completed outcome after request-scoped diagnostic checks.
    pub fn completed(response: WebSearchResponse, credential: &CredentialValue) -> Self {
        credential_safe_transport_outcome(Ok(response), credential)
    }

    /// Builds one failed outcome after request-scoped diagnostic checks.
    pub fn failed(failure: WebSearchTransportFailure, credential: &CredentialValue) -> Self {
        credential_safe_transport_outcome(Err(failure), credential)
    }

    /// Consumes the sanitized transport outcome.
    pub fn into_result(self) -> Result<WebSearchResponse, WebSearchTransportFailure> {
        self.result
    }
}

impl fmt::Debug for WebSearchTransportOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.result {
            Ok(response) => fmt::Debug::fmt(response, formatter),
            Err(failure) => fmt::Debug::fmt(failure, formatter),
        }
    }
}

pub(super) fn credential_safe_transport_outcome(
    outcome: Result<WebSearchResponse, WebSearchTransportFailure>,
    credential: &CredentialValue,
) -> WebSearchTransportOutcome {
    let sanitized = match outcome {
        Ok(response) => {
            let credential_text =
                std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
            if credential_text.is_empty()
                || text_contains_credential_variant(&format!("{response:?}"), credential_text)
            {
                Err(WebSearchTransportFailure::CredentialDiagnosticCollision(
                    WebSearchCredentialDiagnostic {
                        rendered: safe_collision_diagnostic(credential_text),
                        failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                        transport_failure_class: None,
                    },
                ))
            } else {
                Ok(response)
            }
        }
        Err(failure) => Err(credential_safe_transport_failure(failure, credential)),
    };
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let result = if credential_text.is_empty()
        || text_contains_credential_variant(&format!("{sanitized:?}"), credential_text)
    {
        let failure_class = match &sanitized {
            Ok(_) => WebSearchCredentialDiagnosticClass::CallerOrHubBug,
            Err(failure) => transport_failure_diagnostic_class(failure),
        };
        let transport_failure_class = match &sanitized {
            Ok(_) => None,
            Err(failure) => transport_failure_source_class(failure),
        };
        Err(WebSearchTransportFailure::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class,
                transport_failure_class,
            },
        ))
    } else {
        sanitized
    };
    WebSearchTransportOutcome { result }
}

pub(super) fn transport_failure_diagnostic_class(
    failure: &WebSearchTransportFailure,
) -> WebSearchCredentialDiagnosticClass {
    match failure {
        WebSearchTransportFailure::InvalidCredential
        | WebSearchTransportFailure::RequestFailed
        | WebSearchTransportFailure::ProviderRejected(_)
        | WebSearchTransportFailure::InvalidResponse
        | WebSearchTransportFailure::ResponseTooLarge => {
            WebSearchCredentialDiagnosticClass::CallerOrHubBug
        }
        WebSearchTransportFailure::CredentialDiagnosticCollision(diagnostic) => {
            diagnostic.failure_class
        }
        WebSearchTransportFailure::DispatchUnknown => {
            WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous
        }
    }
}

pub(super) fn transport_failure_source_class(
    failure: &WebSearchTransportFailure,
) -> Option<WebSearchTransportFailureClass> {
    match failure {
        WebSearchTransportFailure::CredentialDiagnosticCollision(diagnostic) => {
            diagnostic.transport_failure_class
        }
        WebSearchTransportFailure::InvalidCredential
        | WebSearchTransportFailure::RequestFailed
        | WebSearchTransportFailure::ProviderRejected(_)
        | WebSearchTransportFailure::InvalidResponse
        | WebSearchTransportFailure::ResponseTooLarge
        | WebSearchTransportFailure::DispatchUnknown => Some(failure.class()),
    }
}

pub(super) fn credential_safe_transport_failure(
    failure: WebSearchTransportFailure,
    credential: &CredentialValue,
) -> WebSearchTransportFailure {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let collision_facts = match &failure {
        WebSearchTransportFailure::ProviderRejected(error) => TransportDiagnosticCollisionFacts {
            source_contains_credential: text_contains_credential_variant(
                &format!("{error:?}"),
                credential_text,
            ) || text_contains_credential_variant(
                &error.to_string(),
                credential_text,
            ),
            executor_contains_credential: false,
        },
        WebSearchTransportFailure::InvalidCredential
        | WebSearchTransportFailure::CredentialDiagnosticCollision(_)
        | WebSearchTransportFailure::RequestFailed
        | WebSearchTransportFailure::InvalidResponse
        | WebSearchTransportFailure::ResponseTooLarge => TransportDiagnosticCollisionFacts {
            source_contains_credential: false,
            executor_contains_credential: false,
        },
        WebSearchTransportFailure::DispatchUnknown => {
            let executor_error = WebSearchExecutorError::DispatchUnknown;
            TransportDiagnosticCollisionFacts {
                source_contains_credential: false,
                executor_contains_credential: text_contains_credential_variant(
                    &format!("{executor_error:?}"),
                    credential_text,
                ) || text_contains_credential_variant(
                    &executor_error.to_string(),
                    credential_text,
                ),
            }
        }
    };
    if credential_text.is_empty()
        || text_contains_credential_variant(&format!("{failure:?}"), credential_text)
        || text_contains_credential_variant(&failure.to_string(), credential_text)
        || collision_facts.source_contains_credential
        || collision_facts.executor_contains_credential
    {
        let transport_failure_class = transport_failure_source_class(&failure);
        WebSearchTransportFailure::CredentialDiagnosticCollision(WebSearchCredentialDiagnostic {
            rendered: safe_collision_diagnostic(credential_text),
            failure_class: transport_failure_diagnostic_class(&failure),
            transport_failure_class,
        })
    } else {
        failure
    }
}

struct TransportDiagnosticCollisionFacts {
    source_contains_credential: bool,
    executor_contains_credential: bool,
}
