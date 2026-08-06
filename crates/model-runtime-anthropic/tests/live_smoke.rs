//! Compatibility smoke against the real Anthropic Messages API.
//!
//! Ignored by default: it spends one real model exchange and therefore needs
//! credentials the ordinary Rust workflow never has.
//! `.github/workflows/anthropic-smoke.yml` is the only automated caller: its
//! unprivileged gate rejects changed fork pull requests before the
//! environment-backed smoke job can start.
//!
//! What it proves is protocol compatibility, which is what a public API
//! change actually breaks: `POST /v1/messages` still accepts the request the
//! adapter builds, and its response still decodes as a completed outcome, or
//! as the adapter's downgraded-refusal `ProviderError` shape (buffered
//! delivery cannot prove a refusal arrived only after the complete upload, so
//! `AnthropicRuntime::execute` never returns a raw `Refused` — see
//! `require_decoded_response` below), through the adapter's own types, with
//! usage reported. It deliberately asserts nothing about answer quality.
//!
//! No prompt caching: this smoke sends one small, fixed prompt and nothing
//! else. At that volume a cache write costs more than it could ever recoup,
//! so the request deliberately carries no cache-control breakpoints.
//!
//! Credential discipline: this test never reads, prints, or logs credential
//! material. It resolves `ANTHROPIC_API_KEY` from the environment through
//! the same [`signalbox_model_runtime::CredentialAccess`] boundary
//! production code uses, and the adapter's own redaction sanitizes any
//! provider-controlled text before this test ever sees it.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

#[cfg(test)]
use signalbox_model_runtime::{
    BoundaryLossEvidence, CancellationConfirmedEvidence, CompletionEvidence, CompletionFinish,
    LossCause, NativeErrorFacts, PreparationDefect, PreparationFailure, ProvenUnsentEvidence,
    ProviderErrorEvidence, RefusalEvidence, UnsentCause,
};
use signalbox_model_runtime::{
    CancellationSignal, ConversationMessage, CredentialAccess, CredentialAccessError,
    CredentialAccessFailure, CredentialReference, CredentialValue, ExchangeFacts, ModelOperation,
    ModelRuntime, ModelSettings, PreparationOutcome, ProviderErrorKind, RequestedTarget,
    ResolvedTarget, TerminalEvidence, TokenUsage,
};
use signalbox_model_runtime_anthropic::{AnthropicConfig, AnthropicRuntime};

/// The environment variable this smoke reads its API key from. Configured in
/// CI via the `anthropic-smoke` environment; see
/// `.github/workflows/anthropic-smoke.yml`.
const API_KEY_VARIABLE: &str = "ANTHROPIC_API_KEY";

/// The cheapest current Anthropic model, chosen so a compatibility run costs
/// a small fraction of a cent.
const MODEL: &str = "claude-haiku-4-5";

/// A trivial prompt keeps the exchange to the smallest billable turn that
/// still exercises the whole response envelope.
const PROMPT: &str = "Reply with the single word: ready";

/// A cost cap, not a value Anthropic requires: Haiku completes this trivial
/// prompt well inside it. Named so a reader does not mistake the bare literal
/// for a provider-mandated minimum.
const MAX_OUTPUT_TOKENS: u32 = 64;

#[tokio::test]
#[ignore = "spends one real Anthropic exchange; run only from the gated compatibility smoke"]
async fn the_anthropic_api_completes_one_exchange() {
    let credential_reference = CredentialReference::new("anthropic-smoke");
    let runtime = AnthropicRuntime::new(
        AnthropicConfig::new(),
        EnvironmentCredential {
            variable: API_KEY_VARIABLE,
        },
    )
    .expect("smoke runtime configuration is valid");

    let operation = ModelOperation::new(
        "anthropic-smoke".to_string(),
        credential_reference,
        RequestedTarget::new(MODEL),
        ResolvedTarget::new(MODEL),
        vec![ConversationMessage::user_text(PROMPT)],
        ModelSettings::new(MAX_OUTPUT_TOKENS),
    );

    let prepared = require_prepared(
        runtime
            .prepare(operation, CancellationSignal::never())
            .await,
    );
    let mut observations = Vec::new();
    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    let decoded = require_decoded_response(report.evidence);
    assert_eq!(
        decoded.exchange.http_status,
        Some(200),
        "the adapter no longer records the documented success status"
    );
    assert!(
        decoded.usage.input_tokens.is_some_and(|tokens| tokens > 0),
        "the Messages API no longer reports input usage the adapter can decode"
    );
    // A completion always billed generating at least one output token for
    // this prompt, but a valid refusal can legitimately arrive before any
    // completion token is produced (`output_tokens: Some(0)`): the spec's
    // compatibility-smoke contract promises usage *present*, not positive.
    // Only the completed path can honestly demand a positive count.
    if decoded.completed {
        assert!(
            decoded.usage.output_tokens.is_some_and(|tokens| tokens > 0),
            "the Messages API no longer reports output usage the adapter can decode"
        );
    } else {
        assert!(
            decoded.usage.output_tokens.is_some(),
            "the Messages API no longer reports output usage the adapter can decode"
        );
    }
}

/// Resolves the live API key from the environment, exactly once per
/// operation, through the same boundary production `CredentialAccess`
/// implementations use. Nothing is cached and nothing is logged.
#[derive(Debug)]
struct EnvironmentCredential {
    variable: &'static str,
}

impl CredentialAccess for EnvironmentCredential {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        match std::env::var(self.variable) {
            Ok(value) if !value.is_empty() => Ok(CredentialValue::new(value.into_bytes())),
            _ => Err(CredentialAccessError::new(
                reference.clone(),
                CredentialAccessFailure::Unavailable,
            )),
        }
    }
}

/// `AnthropicPreparedRequest` deliberately implements no diagnostic
/// formatting, so each non-prepared outcome reports only its safe
/// shared-runtime evidence.
#[track_caller]
fn require_prepared<C, P>(outcome: PreparationOutcome<C, P>) -> P {
    match outcome {
        PreparationOutcome::Prepared(prepared) => prepared,
        PreparationOutcome::Cancelled { .. } => panic!("smoke preparation was not cancelled"),
        PreparationOutcome::Failed { failure, .. } => {
            panic!("smoke preparation failed: {failure:?}")
        }
        PreparationOutcome::Defect { defect, .. } => {
            panic!("smoke preparation found a defect: {defect:?}")
        }
    }
}

struct DecodedResponse {
    exchange: ExchangeFacts,
    usage: TokenUsage,
    /// `true` for genuine `Completed` evidence, `false` for the adapter's
    /// downgraded-refusal `ProviderError` shape. The two paths carry
    /// different honest usage guarantees (see the call site).
    completed: bool,
}

/// Accepts a completion, or the adapter's own decoded-refusal shape, as
/// well-formed evidence that the response contract still holds. Only a
/// terminal outcome the adapter never decoded (a transport, protocol, or
/// genuine provider-error class) fails this gate.
///
/// `AnthropicRuntime::execute` never returns a raw `TerminalEvidence::Refused`
/// to its caller: a fully buffered request exposes no independent proof that
/// the response arrived only after the complete upload, so `execute`
/// unconditionally downgrades a decoded refusal into
/// `ProviderError { kind: Unrecognized, .. }` from the same HTTP 200 exchange
/// before returning (`without_unproven_refusal`, runtime.rs:716,729-742; the
/// "Refusal downgrade" rule in `docs/spec/runtime-substrate.md`). Matching the
/// dead `Refused` arm here would make this smoke fail a correctly decoded
/// refusal, so this recognizes what the adapter actually returns instead. The
/// `http_status == 200` guard keeps this arm from also swallowing a genuine
/// unrecognized 4xx/5xx provider error, which the assertions below must still
/// fail on. The returned `completed` flag distinguishes the two accepted
/// shapes for the caller: a refusal can legitimately arrive with zero output
/// tokens (blocked before any completion token was produced), so only the
/// completed path may honestly demand a positive count.
#[track_caller]
fn require_decoded_response(evidence: TerminalEvidence) -> DecodedResponse {
    match evidence {
        TerminalEvidence::Completed(completed) => DecodedResponse {
            exchange: completed.exchange,
            usage: completed.usage,
            completed: true,
        },
        TerminalEvidence::ProviderError(error)
            if error.kind == ProviderErrorKind::Unrecognized
                && error.exchange.http_status == Some(200) =>
        {
            DecodedResponse {
                exchange: error.exchange,
                usage: error.usage,
                completed: false,
            }
        }
        // Adapter-produced evidence is already credential-shape redacted, so
        // printing it here cannot surface credential material. Enumerated
        // explicitly, per docs/style.md's owned-enum rule, so a future
        // `TerminalEvidence` variant fails to compile here instead of
        // silently inheriting this panic path.
        rejected @ (TerminalEvidence::Refused(_)
        | TerminalEvidence::ProviderError(_)
        | TerminalEvidence::CancellationConfirmed(_)
        | TerminalEvidence::ProvenUnsent(_)
        | TerminalEvidence::BoundaryLoss(_)) => {
            panic!("the Anthropic API returned no decoded response: {rejected:?}")
        }
    }
}

/// Credential-free, straight-line coverage for `require_decoded_response`'s
/// branching: one case per accept path and one per rejected variant, so the
/// classifier the paid ignored test relies on is also exercised by the
/// ordinary suite.
#[cfg(test)]
mod require_decoded_response_tests {
    use super::*;

    fn exchange(http_status: u16) -> ExchangeFacts {
        ExchangeFacts {
            http_status: Some(http_status),
            ..ExchangeFacts::default()
        }
    }

    fn usage() -> TokenUsage {
        TokenUsage {
            input_tokens: Some(3),
            output_tokens: Some(1),
            ..TokenUsage::default()
        }
    }

    #[test]
    fn completed_evidence_is_accepted() {
        let expected_exchange = exchange(200);
        let expected_usage = usage();

        let decoded = require_decoded_response(TerminalEvidence::Completed(CompletionEvidence {
            exchange: expected_exchange.clone(),
            message_id: None,
            reported_model: None,
            finish: CompletionFinish::EndTurn,
            content: Vec::new(),
            usage: expected_usage,
        }));

        assert_eq!(decoded.exchange, expected_exchange);
        assert_eq!(decoded.usage, expected_usage);
        assert!(decoded.completed);
    }

    #[test]
    fn downgraded_refusal_provider_error_is_accepted() {
        // The exact shape `without_unproven_refusal` constructs: `kind:
        // Unrecognized` carried by the same HTTP 200 exchange.
        let expected_exchange = exchange(200);
        let expected_usage = usage();

        let decoded =
            require_decoded_response(TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: expected_exchange.clone(),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                native: NativeErrorFacts {
                    error_token: Some("refusal".to_string()),
                    error_code: None,
                    message: None,
                },
                usage: expected_usage,
            }));

        assert_eq!(decoded.exchange, expected_exchange);
        assert_eq!(decoded.usage, expected_usage);
        assert!(!decoded.completed);
    }

    #[test]
    fn downgraded_refusal_with_zero_output_tokens_is_accepted() {
        // A refusal can be blocked before any completion token is produced —
        // `output_tokens: Some(0)` is a valid, honest report here, unlike for
        // a genuine completion. The classifier must still accept it; only
        // the caller's `completed`-gated assertion (see
        // `the_anthropic_api_completes_one_exchange`) tells the two apart.
        let expected_exchange = exchange(200);
        let expected_usage = TokenUsage {
            input_tokens: Some(3),
            output_tokens: Some(0),
            ..TokenUsage::default()
        };

        let decoded =
            require_decoded_response(TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: expected_exchange.clone(),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                native: NativeErrorFacts {
                    error_token: Some("refusal".to_string()),
                    error_code: None,
                    message: None,
                },
                usage: expected_usage,
            }));

        assert_eq!(decoded.exchange, expected_exchange);
        assert_eq!(decoded.usage, expected_usage);
        assert!(!decoded.completed);
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn raw_refused_panics() {
        let _ = require_decoded_response(TerminalEvidence::Refused(RefusalEvidence {
            exchange: exchange(200),
            message_id: None,
            reported_model: None,
            content: Vec::new(),
            usage: usage(),
        }));
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn unrecognized_provider_error_from_a_non_200_status_panics() {
        // Same `kind` as the accepted downgraded-refusal shape, but from a
        // real error status: the `http_status == 200` guard must keep this
        // from being swallowed as a well-formed decode.
        let _ = require_decoded_response(TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: exchange(500),
            reported_model: None,
            kind: ProviderErrorKind::Unrecognized,
            native: NativeErrorFacts::default(),
            usage: TokenUsage::unreported(),
        }));
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn recognized_provider_error_panics() {
        let _ = require_decoded_response(TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: exchange(401),
            reported_model: None,
            kind: ProviderErrorKind::CredentialRejected,
            native: NativeErrorFacts::default(),
            usage: TokenUsage::unreported(),
        }));
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn cancellation_confirmed_panics() {
        let _ = require_decoded_response(TerminalEvidence::CancellationConfirmed(
            CancellationConfirmedEvidence {
                exchange: exchange(200),
                reported_model: None,
                native: NativeErrorFacts::default(),
            },
        ));
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn proven_unsent_panics() {
        let _ = require_decoded_response(TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
            cause: UnsentCause::CancelledBeforeSend,
        }));
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn boundary_loss_panics() {
        let _ = require_decoded_response(TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
            cause: LossCause::UnexpectedHttpStatus,
            exchange: exchange(200),
            reported_model: None,
            finish_reported: None,
            usage: TokenUsage::unreported(),
        }));
    }
}

/// Credential-free, straight-line coverage for `require_prepared`'s
/// branching: it is generic over the prepared capability type, so a
/// placeholder `u32` capability exercises every `PreparationOutcome` variant
/// without needing a real adapter request.
#[cfg(test)]
mod require_prepared_tests {
    use super::*;

    #[test]
    fn prepared_outcome_returns_its_capability() {
        let outcome: PreparationOutcome<String, u32> = PreparationOutcome::Prepared(7);

        assert_eq!(require_prepared(outcome), 7);
    }

    #[test]
    #[should_panic(expected = "smoke preparation was not cancelled")]
    fn cancelled_outcome_panics() {
        let outcome: PreparationOutcome<String, u32> = PreparationOutcome::Cancelled {
            correlation: "call-1".to_string(),
        };

        let _ = require_prepared(outcome);
    }

    #[test]
    #[should_panic(expected = "smoke preparation failed")]
    fn failed_outcome_panics() {
        let outcome: PreparationOutcome<String, u32> = PreparationOutcome::Failed {
            correlation: "call-1".to_string(),
            failure: PreparationFailure::UnsupportedOperation {
                detail: "synthetic".to_string(),
            },
        };

        let _ = require_prepared(outcome);
    }

    #[test]
    #[should_panic(expected = "smoke preparation found a defect")]
    fn defect_outcome_panics() {
        let outcome: PreparationOutcome<String, u32> = PreparationOutcome::Defect {
            correlation: "call-1".to_string(),
            defect: PreparationDefect::SerializationFailed {
                detail: "synthetic".to_string(),
            },
        };

        let _ = require_prepared(outcome);
    }
}
