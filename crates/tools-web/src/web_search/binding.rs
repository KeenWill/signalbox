use signalbox_application::{
    CorrelatedToolExecutorEvidence, ToolExecutionInvocation, ToolExecutorEvidence,
};
use signalbox_domain::{ToolAttemptDispatchCorrelation, ToolExecutionErrorDetail};
use signalbox_model_runtime::CredentialValue;
use std::fmt;

use super::{diagnostic::*, executor::*, redaction::*, request::*, text_decoding::*};

pub(super) const MAX_BOUND_EVIDENCE_DEBUG_BYTES: usize = MAX_CREDENTIAL_BYTES * 2;

pub(super) const MAX_OVERSIZED_CREDENTIAL_INSPECTION_BYTES: usize = MAX_BOUND_EVIDENCE_DEBUG_BYTES;

pub(super) struct BoundedCredentialVariants {
    pub(super) variants: Vec<String>,
    pub(super) complete: bool,
}

impl BoundedCredentialVariants {
    pub(super) fn try_from_oversized(credential: &str) -> Option<Self> {
        if credential.len() > MAX_OVERSIZED_CREDENTIAL_INSPECTION_BYTES {
            return None;
        }
        let mut variants = Vec::new();
        retain_bounded_credential_variant(&mut variants, credential);
        let Some(first_decoded) = decode_reversible_text_once(credential) else {
            return Some(Self {
                variants,
                complete: false,
            });
        };
        if first_decoded.change == ReversibleTextChange::Unchanged {
            return Some(Self {
                variants,
                complete: true,
            });
        }
        let mut decoded = first_decoded.text;
        retain_bounded_credential_variant(&mut variants, &decoded);
        for _ in 1..MAX_REVERSIBLE_DECODE_PASSES {
            let Some(next) = decode_reversible_text_once(&decoded) else {
                return Some(Self {
                    variants,
                    complete: false,
                });
            };
            if next.change == ReversibleTextChange::Unchanged {
                return Some(Self {
                    variants,
                    complete: true,
                });
            }
            retain_bounded_credential_variant(&mut variants, &next.text);
            decoded = next.text;
        }
        let complete = decode_reversible_text_once(&decoded)
            .is_some_and(|decoded| decoded.change == ReversibleTextChange::Unchanged);
        Some(Self { variants, complete })
    }

    pub(super) fn collides(&self, rendered: &str) -> bool {
        if !self.complete || rendered.len() > MAX_BOUND_EVIDENCE_DEBUG_BYTES {
            return true;
        }
        self.variants
            .iter()
            .any(|credential| bound_diagnostic_contains_credential(rendered, credential))
    }
}

pub(super) fn retain_bounded_credential_variant(variants: &mut Vec<String>, candidate: &str) {
    if !candidate.is_empty()
        && candidate.len() <= MAX_BOUND_EVIDENCE_DEBUG_BYTES
        && !variants.iter().any(|retained| retained == candidate)
    {
        variants.push(String::from(candidate));
    }
    let normalized = unicode_case_folded_nfd(candidate);
    if !normalized.is_empty()
        && normalized.len() <= MAX_BOUND_EVIDENCE_DEBUG_BYTES
        && !variants.iter().any(|retained| retained == &normalized)
    {
        variants.push(normalized);
    }
}

pub(super) enum BoundCredentialCheck {
    None,
    Exact(CredentialValue),
    BoundedVariants(BoundedCredentialVariants),
}

pub(super) fn bind_request_outcome(
    invocation: ToolExecutionInvocation,
    outcome: WebSearchRequestOutcome,
) -> Result<CorrelatedToolExecutorEvidence, WebSearchExecutorError> {
    let evidence = match outcome {
        WebSearchRequestOutcome::Evidence(evidence) => evidence,
        WebSearchRequestOutcome::CredentialFreeError(error) => return Err(error),
        WebSearchRequestOutcome::Error { error, credential } => {
            let credential_text =
                std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
            let rendered_result = format!(
                "{:?}",
                Result::<&CorrelatedToolExecutorEvidence, _>::Err(&error)
            );
            if !credential_text.is_empty()
                && !text_contains_credential_variant(&rendered_result, credential_text)
            {
                return Err(error);
            }
            if executor_error_diagnostic_class(&error)
                == WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous
            {
                return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
                    WebSearchCredentialDiagnostic {
                        rendered: safe_collision_diagnostic(credential_text),
                        failure_class:
                            WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous,
                        transport_failure_class: executor_error_transport_failure_class(&error),
                    },
                ));
            }
            let fallback = invocation.bind(ToolExecutorEvidence::KnownFailed { detail: None });
            let rendered_fallback =
                format!("{:?}", Result::<_, &WebSearchExecutorError>::Ok(&fallback));
            if !credential_text.is_empty()
                && !text_contains_credential_variant(&rendered_fallback, credential_text)
            {
                return Ok(fallback);
            }
            return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
                WebSearchCredentialDiagnostic {
                    rendered: safe_collision_diagnostic(credential_text),
                    failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                    transport_failure_class: executor_error_transport_failure_class(&error),
                },
            ));
        }
    };
    let (evidence, credential) = match evidence {
        WebSearchRequestEvidence::Uncredentialed(evidence) => {
            (evidence, BoundCredentialCheck::None)
        }
        WebSearchRequestEvidence::Credentialed {
            evidence,
            credential,
        } => (evidence, BoundCredentialCheck::Exact(credential)),
        WebSearchRequestEvidence::BoundedCredentialVariants {
            evidence,
            credential,
        } => (evidence, BoundCredentialCheck::BoundedVariants(credential)),
    };
    let has_dynamic_known_failure_detail = match &evidence {
        ToolExecutorEvidence::CompletedText(_) | ToolExecutorEvidence::Ambiguous => false,
        ToolExecutorEvidence::KnownFailed { detail } => detail.is_some(),
    };
    let fallback_invocation = has_dynamic_known_failure_detail.then(|| invocation.clone());
    let bound = invocation.bind(evidence);
    let rendered_result = format!("{:?}", Result::<_, &WebSearchExecutorError>::Ok(&bound));
    match credential {
        BoundCredentialCheck::None => Ok(bound),
        BoundCredentialCheck::BoundedVariants(credential) => {
            if credential.collides(&rendered_result) {
                if let Some(fallback_invocation) = fallback_invocation {
                    let fallback = fallback_invocation
                        .bind(ToolExecutorEvidence::KnownFailed { detail: None });
                    let rendered_fallback =
                        format!("{:?}", Result::<_, &WebSearchExecutorError>::Ok(&fallback));
                    if !credential.collides(&rendered_fallback) {
                        return Ok(fallback);
                    }
                }
                return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
                    WebSearchCredentialDiagnostic {
                        rendered: String::new(),
                        failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                        transport_failure_class: None,
                    },
                ));
            }
            Ok(bound)
        }
        BoundCredentialCheck::Exact(credential) => {
            let credential_text =
                std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
            if exact_bound_diagnostic_collides(&rendered_result, credential_text) {
                if let Some(fallback_invocation) = fallback_invocation {
                    let fallback = fallback_invocation
                        .bind(ToolExecutorEvidence::KnownFailed { detail: None });
                    let rendered_fallback =
                        format!("{:?}", Result::<_, &WebSearchExecutorError>::Ok(&fallback));
                    if !exact_bound_diagnostic_collides(&rendered_fallback, credential_text) {
                        return Ok(fallback);
                    }
                }
                return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
                    WebSearchCredentialDiagnostic {
                        rendered: safe_collision_diagnostic(credential_text),
                        failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                        transport_failure_class: None,
                    },
                ));
            }
            Ok(bound)
        }
    }
}

pub(super) fn exact_bound_diagnostic_collides(rendered: &str, credential: &str) -> bool {
    if credential.is_empty() {
        return true;
    }
    bound_diagnostic_contains_credential(rendered, credential)
}

pub(super) fn fixed_bound_evidence_token_collides(scrubber: &CredentialScrubber) -> bool {
    let mut probe = ToolExecutorEvidence::CompletedText(String::new());
    loop {
        let rendered = format!("{probe:?}");
        if scrubber.contains_credential(&rendered) {
            return true;
        }
        let Some(next) = next_fixed_bound_evidence_probe(&probe) else {
            return false;
        };
        probe = next;
    }
}

pub(super) fn fixed_populated_failure_detail_collides(
    scrubber: &CredentialScrubber,
    correlation: &ToolAttemptDispatchCorrelation,
    details: &[&ToolExecutionErrorDetail],
) -> bool {
    details.iter().any(|detail| {
        let evidence = ToolExecutorEvidence::KnownFailed {
            detail: Some((*detail).clone()),
        };
        bound_wrapper_evidence_collides(scrubber, correlation, &evidence)
    })
}

pub(super) fn fixed_bound_wrapper_token_collides(
    scrubber: &CredentialScrubber,
    correlation: &ToolAttemptDispatchCorrelation,
) -> bool {
    if fixed_success_payloads().any(|payload| {
        if scrubber.contains_credential(&payload) {
            return false;
        }
        let evidence = ToolExecutorEvidence::CompletedText(payload);
        bound_wrapper_evidence_collides(scrubber, correlation, &evidence)
    }) {
        return true;
    }
    let mut evidence = ToolExecutorEvidence::CompletedText(String::new());
    loop {
        if bound_wrapper_evidence_collides(scrubber, correlation, &evidence) {
            return true;
        }
        let Some(next) = next_fixed_bound_evidence_probe(&evidence) else {
            return false;
        };
        evidence = next;
    }
}

pub(super) fn bound_wrapper_evidence_collides(
    scrubber: &CredentialScrubber,
    correlation: &ToolAttemptDispatchCorrelation,
    evidence: &ToolExecutorEvidence,
) -> bool {
    let probe = CorrelatedToolExecutorEvidenceDebugProbe {
        correlation,
        evidence,
    };
    let rendered = format!("{:?}", Result::<_, &WebSearchExecutorError>::Ok(&probe));
    scrubber.contains_credential(&rendered)
}

pub(super) struct CorrelatedToolExecutorEvidenceDebugProbe<'a> {
    pub(super) correlation: &'a ToolAttemptDispatchCorrelation,
    pub(super) evidence: &'a ToolExecutorEvidence,
}

impl fmt::Debug for CorrelatedToolExecutorEvidenceDebugProbe<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CorrelatedToolExecutorEvidence")
            .field(
                "fence",
                &IssuedExecutorFenceDebugProbe {
                    correlation: self.correlation,
                },
            )
            .field("evidence", self.evidence)
            .finish()
    }
}

pub(super) struct IssuedExecutorFenceDebugProbe<'a> {
    pub(super) correlation: &'a ToolAttemptDispatchCorrelation,
}

impl fmt::Debug for IssuedExecutorFenceDebugProbe<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedExecutorFence")
            .field("correlation", self.correlation)
            .finish()
    }
}

pub(super) fn bound_diagnostic_contains_credential(rendered: &str, credential: &str) -> bool {
    if text_contains_credential_variant(rendered, credential) {
        return true;
    }
    let trimmed = credential.trim_matches(|character| character == ' ' || character == '\t');
    trimmed != credential
        && !trimmed.is_empty()
        && text_contains_credential_variant(rendered, trimmed)
}

pub(super) fn next_fixed_bound_evidence_probe(
    evidence: &ToolExecutorEvidence,
) -> Option<ToolExecutorEvidence> {
    match evidence {
        ToolExecutorEvidence::CompletedText(_) => {
            Some(ToolExecutorEvidence::KnownFailed { detail: None })
        }
        ToolExecutorEvidence::KnownFailed { .. } => Some(ToolExecutorEvidence::Ambiguous),
        ToolExecutorEvidence::Ambiguous => None,
    }
}
