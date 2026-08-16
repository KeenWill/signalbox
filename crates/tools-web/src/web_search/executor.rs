use signalbox_application::{
    CorrelatedToolExecutorEvidence, ToolExecutionInvocation, ToolExecutor, ToolExecutorEvidence,
};
use signalbox_domain::{ToolAttemptDispatchCorrelation, ToolExecutionErrorDetail};
use signalbox_model_runtime::{CredentialAccess, CredentialValue};
use std::fmt;

use super::{
    arguments::*, binding::*, diagnostic::*, egress::*, evidence::*, redaction::*, request::*,
    telemetry::*, text_decoding::*, transport::*, transport_failure::*,
};

pub(super) const DYNAMIC_SUCCESS_VALUE_MARKERS: [&str; 6] = [
    "SIGNALBOX_DYNAMIC_TITLE_MARKER_ONE",
    "SIGNALBOX_DYNAMIC_URL_MARKER_ONE",
    "SIGNALBOX_DYNAMIC_SNIPPET_MARKER_ONE",
    "SIGNALBOX_DYNAMIC_TITLE_MARKER_TWO",
    "SIGNALBOX_DYNAMIC_URL_MARKER_TWO",
    "SIGNALBOX_DYNAMIC_SNIPPET_MARKER_TWO",
];

pub(super) fn dynamic_success_field_boundary_may_collide(
    scrubber: &CredentialScrubber,
    correlation: &ToolAttemptDispatchCorrelation,
) -> bool {
    let Some(payload) = dynamic_success_payload_probe() else {
        return true;
    };
    let evidence = ToolExecutorEvidence::CompletedText(payload.clone());
    let bound = CorrelatedToolExecutorEvidenceDebugProbe {
        correlation,
        evidence: &evidence,
    };
    let rendered_bound = format!("{:?}", Result::<_, &WebSearchExecutorError>::Ok(&bound));
    [payload.as_str(), rendered_bound.as_str()]
        .into_iter()
        .any(|rendered| dynamic_success_value_boundary_may_collide(rendered, scrubber))
}

pub(super) fn dynamic_success_payload_probe() -> Option<String> {
    canonical_json_string(serde_json::json!({
        "results": [
            RenderedSearchResult {
                title: String::from(DYNAMIC_SUCCESS_VALUE_MARKERS[0]),
                url: String::from(DYNAMIC_SUCCESS_VALUE_MARKERS[1]),
                snippet: String::from(DYNAMIC_SUCCESS_VALUE_MARKERS[2]),
            },
            RenderedSearchResult {
                title: String::from(DYNAMIC_SUCCESS_VALUE_MARKERS[3]),
                url: String::from(DYNAMIC_SUCCESS_VALUE_MARKERS[4]),
                snippet: String::from(DYNAMIC_SUCCESS_VALUE_MARKERS[5]),
            },
        ],
        "truncated": false,
    }))
    .ok()
}

pub(super) fn dynamic_success_value_boundary_may_collide(
    rendered: &str,
    scrubber: &CredentialScrubber,
) -> bool {
    let normalized_credentials = scrubber
        .output_collision_variants()
        .map(unicode_case_folded_nfd)
        .collect::<Vec<_>>();
    let mut marker_spans = Vec::with_capacity(DYNAMIC_SUCCESS_VALUE_MARKERS.len());
    for marker in DYNAMIC_SUCCESS_VALUE_MARKERS {
        let Some((prefix, suffix)) = rendered.split_once(marker) else {
            return true;
        };
        if suffix.contains(marker) {
            return true;
        }
        let normalized_prefix = unicode_case_folded_nfd(prefix);
        let normalized_suffix = unicode_case_folded_nfd(suffix);
        if normalized_credentials.iter().any(|credential| {
            credential.char_indices().skip(1).any(|(split, _)| {
                normalized_prefix.ends_with(&credential[..split])
                    || normalized_suffix.starts_with(&credential[split..])
            })
        }) {
            return true;
        }
        marker_spans.push((prefix.len(), prefix.len() + marker.len()));
    }
    marker_spans.sort_unstable_by_key(|span| span.0);
    marker_spans.windows(2).any(|adjacent| {
        rendered
            .get(adjacent[0].1..adjacent[1].0)
            .is_none_or(|separator| {
                scrubber
                    .output_collision_variants()
                    .any(|credential| unicode_case_insensitive_contains(credential, separator))
            })
    })
}

/// Credential-resolving daemon-local web-search executor.
#[derive(Clone)]
pub struct WebSearchExecutor<Credentials, Transport> {
    pub(super) credentials: Credentials,
    pub(super) transport: Transport,
    pub(super) configuration: WebSearchConfiguration,
    pub(super) credential_unavailable_detail: ToolExecutionErrorDetail,
    pub(super) request_failed_detail: ToolExecutionErrorDetail,
    pub(super) provider_rejected_detail: ToolExecutionErrorDetail,
    pub(super) invalid_response_detail: ToolExecutionErrorDetail,
}

impl<Credentials, Transport> fmt::Debug for WebSearchExecutor<Credentials, Transport> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSearchExecutor")
            .field("configuration", &self.configuration)
            .field("credentials", &"[injected]")
            .field("transport", &"[injected]")
            .finish_non_exhaustive()
    }
}

pub(super) fn executor_debug_contains_credential<Credentials, Transport>(
    executor: &WebSearchExecutor<Credentials, Transport>,
    credential: &str,
) -> bool {
    text_contains_credential_variant(&format!("{executor:?}"), credential)
}

impl<Credentials, Transport> WebSearchExecutor<Credentials, Transport>
where
    Credentials: CredentialAccess,
    Transport: WebSearchTransport,
{
    pub(super) async fn execute_request(
        &mut self,
        request: WebSearchRequest,
        correlation: &ToolAttemptDispatchCorrelation,
    ) -> WebSearchRequestOutcome {
        let credential = match self
            .credentials
            .resolve(&self.configuration.credential_reference)
            .await
        {
            Ok(credential) => credential,
            Err(error) => {
                report_credential_access_failure(&error, correlation);
                return WebSearchRequestOutcome::Evidence(
                    WebSearchRequestEvidence::Uncredentialed(ToolExecutorEvidence::KnownFailed {
                        detail: Some(self.credential_unavailable_detail.clone()),
                    }),
                );
            }
        };
        let Some(scrubber) = CredentialScrubber::try_new(&credential) else {
            let _reporting = report_credential_value_failure(correlation, &credential);
            let credential_text =
                std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
            let credential_is_oversized = credential.expose_bytes().len() > MAX_CREDENTIAL_BYTES;
            let detail = (credential_is_oversized
                || !fixed_outer_error_debug_may_contain(credential_text))
            .then(|| self.credential_unavailable_detail.clone());
            let evidence = ToolExecutorEvidence::KnownFailed { detail };
            if credential_is_oversized && !credential_text.is_empty() {
                let Some(credential) =
                    BoundedCredentialVariants::try_from_oversized(credential_text)
                else {
                    return WebSearchRequestOutcome::CredentialFreeError(
                        WebSearchExecutorError::CredentialDiagnosticCollision(
                            WebSearchCredentialDiagnostic {
                                rendered: String::new(),
                                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                                transport_failure_class: None,
                            },
                        ),
                    );
                };
                return WebSearchRequestOutcome::Evidence(
                    WebSearchRequestEvidence::BoundedCredentialVariants {
                        evidence,
                        credential,
                    },
                );
            }
            let retain_for_bound_diagnostic = credential.expose_bytes().len()
                <= MAX_CREDENTIAL_BYTES
                && !credential_text.is_empty();
            return if retain_for_bound_diagnostic {
                WebSearchRequestOutcome::Evidence(WebSearchRequestEvidence::Credentialed {
                    evidence,
                    credential,
                })
            } else {
                WebSearchRequestOutcome::Evidence(WebSearchRequestEvidence::Uncredentialed(
                    evidence,
                ))
            };
        };
        let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
        if fixed_populated_failure_detail_collides(
            &scrubber,
            correlation,
            &[
                &self.credential_unavailable_detail,
                &self.request_failed_detail,
                &self.provider_rejected_detail,
                &self.invalid_response_detail,
            ],
        ) {
            return WebSearchRequestOutcome::Evidence(WebSearchRequestEvidence::Credentialed {
                evidence: ToolExecutorEvidence::KnownFailed { detail: None },
                credential,
            });
        }
        if fixed_bound_evidence_token_collides(&scrubber)
            || fixed_bound_wrapper_token_collides(&scrubber, correlation)
        {
            return WebSearchRequestOutcome::Error {
                error: WebSearchExecutorError::CredentialDiagnosticCollision(
                    WebSearchCredentialDiagnostic {
                        rendered: safe_collision_diagnostic(credential_text),
                        failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                        transport_failure_class: None,
                    },
                ),
                credential,
            };
        }
        if query_contains_credential(request.query(), credential_text)
            || fixed_request_metadata_contains_credential(&request, credential_text)
            || dynamic_success_field_boundary_may_collide(&scrubber, correlation)
            || serialized_request_url_contains_credential(&request, credential_text)
            || credential_debug_contains_credential(&credential, credential_text)
            || request_credential_debug_contains_credential(&request, &credential, credential_text)
            || executor_debug_contains_credential(self, credential_text)
        {
            let outcome = known_failure_evidence(self.request_failed_detail.clone(), &scrubber);
            return match outcome {
                Ok(evidence) => {
                    WebSearchRequestOutcome::Evidence(WebSearchRequestEvidence::Credentialed {
                        evidence,
                        credential,
                    })
                }
                Err(error) => WebSearchRequestOutcome::Error { error, credential },
            };
        }
        let transport_result = self
            .transport
            .search(request, &credential)
            .await
            .into_result();
        if let Err(failure) = &transport_result
            && let Some(failure_class) = failure.response_body_failure_class()
        {
            let _reporting = report_response_body_failure(failure_class, correlation, &credential);
        }
        if let Err(failure) = &transport_result {
            let _reporting = report_transport_failure(failure, correlation, &credential);
        }
        let outcome = match transport_result {
            Ok(response) => match success_evidence(response, &scrubber) {
                Ok(evidence) => Ok(evidence),
                Err(WebSearchExecutorError::EvidenceEncoding) => {
                    let _reporting = report_response_sanitization_failure(correlation, &credential);
                    Ok(self.invalid_response_evidence(&scrubber))
                }
                Err(error) => Err(error),
            },
            Err(WebSearchTransportFailure::InvalidCredential) => {
                known_failure_evidence(self.credential_unavailable_detail.clone(), &scrubber)
            }
            Err(WebSearchTransportFailure::CredentialDiagnosticCollision(diagnostic)) => {
                self.credential_diagnostic_evidence(diagnostic, &scrubber)
            }
            Err(WebSearchTransportFailure::RequestFailed) => {
                known_failure_evidence(self.request_failed_detail.clone(), &scrubber)
            }
            Err(WebSearchTransportFailure::ProviderRejected(error)) => {
                provider_error_detail(error, &scrubber)
                    .map(|detail| ToolExecutorEvidence::KnownFailed { detail })
            }
            Err(
                WebSearchTransportFailure::InvalidResponse
                | WebSearchTransportFailure::ResponseTooLarge,
            ) => known_failure_evidence(self.invalid_response_detail.clone(), &scrubber),
            Err(WebSearchTransportFailure::DispatchUnknown) => {
                Err(WebSearchExecutorError::DispatchUnknown)
            }
        };
        match outcome {
            Ok(evidence) => {
                WebSearchRequestOutcome::Evidence(WebSearchRequestEvidence::Credentialed {
                    evidence,
                    credential,
                })
            }
            Err(error) => WebSearchRequestOutcome::Error {
                error: credential_safe_executor_error(error, &credential),
                credential,
            },
        }
    }

    pub(super) fn credential_diagnostic_evidence(
        &self,
        diagnostic: WebSearchCredentialDiagnostic,
        scrubber: &CredentialScrubber,
    ) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
        match diagnostic.transport_failure_class {
            Some(WebSearchTransportFailureClass::InvalidCredential) => {
                known_failure_evidence(self.credential_unavailable_detail.clone(), scrubber)
            }
            Some(WebSearchTransportFailureClass::RequestFailed) => {
                known_failure_evidence(self.request_failed_detail.clone(), scrubber)
            }
            Some(WebSearchTransportFailureClass::ProviderRejected) => {
                known_failure_evidence(self.provider_rejected_detail.clone(), scrubber)
            }
            Some(
                WebSearchTransportFailureClass::InvalidResponse
                | WebSearchTransportFailureClass::ResponseTooLarge,
            ) => known_failure_evidence(self.invalid_response_detail.clone(), scrubber),
            Some(WebSearchTransportFailureClass::DispatchUnknown) => Err(
                WebSearchExecutorError::CredentialDiagnosticCollision(diagnostic),
            ),
            Some(WebSearchTransportFailureClass::CredentialDiagnosticCollision) | None => {
                match diagnostic.failure_class {
                    WebSearchCredentialDiagnosticClass::CallerOrHubBug => {
                        known_failure_evidence(self.credential_unavailable_detail.clone(), scrubber)
                    }
                    WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous => Err(
                        WebSearchExecutorError::CredentialDiagnosticCollision(diagnostic),
                    ),
                }
            }
        }
    }

    pub(super) fn invalid_response_evidence(
        &self,
        scrubber: &CredentialScrubber,
    ) -> ToolExecutorEvidence {
        let detail = (!scrubber.contains_credential(self.invalid_response_detail.as_str()))
            .then(|| self.invalid_response_detail.clone());
        ToolExecutorEvidence::KnownFailed { detail }
    }
}

pub(super) enum WebSearchRequestOutcome {
    Evidence(WebSearchRequestEvidence),
    CredentialFreeError(WebSearchExecutorError),
    Error {
        error: WebSearchExecutorError,
        credential: CredentialValue,
    },
}

#[cfg(test)]
impl WebSearchRequestOutcome {
    pub(super) fn into_result(self) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
        match self {
            Self::Evidence(evidence) => Ok(evidence.into_evidence()),
            Self::CredentialFreeError(error) => Err(error),
            Self::Error { error, .. } => Err(error),
        }
    }
}

pub(super) enum WebSearchRequestEvidence {
    Uncredentialed(ToolExecutorEvidence),
    Credentialed {
        evidence: ToolExecutorEvidence,
        credential: CredentialValue,
    },
    BoundedCredentialVariants {
        evidence: ToolExecutorEvidence,
        credential: BoundedCredentialVariants,
    },
}

impl fmt::Debug for WebSearchRequestEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uncredentialed(evidence)
            | Self::Credentialed { evidence, .. }
            | Self::BoundedCredentialVariants { evidence, .. } => {
                fmt::Debug::fmt(evidence, formatter)
            }
        }
    }
}

#[cfg(test)]
impl WebSearchRequestEvidence {
    pub(super) fn into_evidence(self) -> ToolExecutorEvidence {
        match self {
            Self::Uncredentialed(evidence)
            | Self::Credentialed { evidence, .. }
            | Self::BoundedCredentialVariants { evidence, .. } => evidence,
        }
    }
}

impl<Credentials, Transport> ToolExecutor for WebSearchExecutor<Credentials, Transport>
where
    Credentials: CredentialAccess,
    Transport: WebSearchTransport,
{
    type Error = WebSearchExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let request = decode_arguments_for_provider(
            invocation.request().arguments(),
            self.configuration.provider,
        )
        .map_err(|_| WebSearchExecutorError::ArgumentValidationDrift)?;
        let correlation = invocation.correlation();
        let outcome = self.execute_request(request, &correlation).await;
        bind_request_outcome(invocation, outcome)
    }
}
