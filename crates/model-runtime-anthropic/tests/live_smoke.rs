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
//! as the adapter's downgraded-refusal `ProviderError` shape (this transport
//! exposes no independent proof that a response arrived only after the
//! complete request was sent, so `AnthropicRuntime::execute` never returns a
//! raw `Refused` — see `require_decoded_response` below), through the
//! adapter's own types, with usage reported. It deliberately asserts nothing
//! about answer quality.
//!
//! Streamed delivery: the operation requests `DeliveryMode::Streamed`,
//! matching the only delivery mode production ever selects
//! (`RuntimeModelCallProvider` in `crates/model-provider-runtime` sets it
//! unconditionally), so this smoke exercises the deployed SSE decoder rather
//! than the buffered path production never uses.
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
use std::time::Duration;

use signalbox_model_runtime::{
    CancellationSignal, ConversationMessage, CredentialAccess, CredentialAccessError,
    CredentialAccessFailure, CredentialReference, CredentialValue, DeliveryMode, ExchangeFacts,
    ModelOperation, ModelRuntime, ModelSettings, PreparationOutcome, ProviderErrorKind,
    RequestedTarget, ResolvedTarget, TerminalEvidence, TokenUsage,
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

/// Bounds the one exchange well inside the workflow job's 10-minute budget.
/// `AnthropicConfig::new()`'s own default (10 minutes) leaves no headroom for
/// dependency setup and compilation ahead of it in that same job: if the
/// provider ever stalls near the adapter's default, GitHub's job timeout
/// fires first and kills the job before the adapter's own typed timeout
/// evidence can be produced. A trivial one-word exchange healthy enough to
/// prove compatibility completes in seconds; two minutes is generous
/// slack, not a tight bound.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// Applies this smoke's timeout policy on top of the adapter's documented
/// defaults.
fn anthropic_config() -> AnthropicConfig {
    let mut config = AnthropicConfig::new();
    config.exchange_timeout = EXCHANGE_TIMEOUT;
    config
}

#[tokio::test]
#[ignore = "spends one real Anthropic exchange; run only from the gated compatibility smoke"]
async fn the_anthropic_api_completes_one_exchange() {
    let credential_reference = CredentialReference::new("anthropic-smoke");
    let runtime = AnthropicRuntime::new(
        anthropic_config(),
        EnvironmentCredential {
            variable: API_KEY_VARIABLE,
        },
    )
    .expect("smoke runtime configuration is valid");

    let mut operation = ModelOperation::new(
        "anthropic-smoke".to_string(),
        credential_reference,
        RequestedTarget::new(MODEL),
        ResolvedTarget::new(MODEL),
        vec![ConversationMessage::user_text(PROMPT)],
        ModelSettings::new(MAX_OUTPUT_TOKENS),
    );
    // Matches the only delivery mode production ever selects (see the module
    // doc comment); a buffered exchange here would prove nothing about the
    // deployed SSE decoder.
    operation.delivery = DeliveryMode::Streamed;

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
    assert_well_formed_response(&decoded);
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
/// `ProviderError { kind: Unrecognized, native: { error_token: Some("refusal"), .. }, .. }`
/// from the same HTTP 200 exchange before returning (`without_unproven_refusal`,
/// runtime.rs:716,729-742; the "Refusal downgrade" rule in
/// `docs/spec/runtime-substrate.md`). Matching the dead `Refused` arm here
/// would make this smoke fail a correctly decoded refusal, so this recognizes
/// what the adapter actually returns instead. The guard checks all three
/// facts `without_unproven_refusal` sets — `kind`, `http_status == 200`, and
/// `native.error_token == "refusal"` — not just the first two: a future
/// HTTP-200 `Unrecognized` provider error reached some other way would share
/// `kind` and status but not this exact token, and must still fail as a
/// genuine, undecoded provider error rather than being waved through as a
/// refusal it never was.
#[track_caller]
fn require_decoded_response(evidence: TerminalEvidence) -> DecodedResponse {
    match evidence {
        TerminalEvidence::Completed(completed) => DecodedResponse {
            exchange: completed.exchange,
            usage: completed.usage,
        },
        TerminalEvidence::ProviderError(error)
            if error.kind == ProviderErrorKind::Unrecognized
                && error.exchange.http_status == Some(200)
                && error.native.error_token.as_deref() == Some("refusal") =>
        {
            DecodedResponse {
                exchange: error.exchange,
                usage: error.usage,
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

/// Asserts a decoded response is well-formed under the compatibility-smoke
/// contract in `docs/spec/runtime-substrate.md`: a definitive success status
/// and provider-reported input/output usage *present*. Input tokens must
/// also be positive — a request that reached the model always billed at
/// least one — but output tokens are asserted only present, not positive: a
/// valid `Completed` response can legitimately report zero output tokens
/// (the adapter's own streamed fixtures cover an `end_turn` with
/// `output_tokens: Some(0)` as `Completed`), and a downgraded-refusal
/// `ProviderError` can be blocked before any completion token is produced.
/// Straight-line and credential-free: no test body branches on which
/// accepted shape arrived.
#[track_caller]
fn assert_well_formed_response(decoded: &DecodedResponse) {
    assert_eq!(
        decoded.exchange.http_status,
        Some(200),
        "the adapter no longer records the documented success status"
    );
    assert!(
        decoded.usage.input_tokens.is_some_and(|tokens| tokens > 0),
        "the Messages API no longer reports input usage the adapter can decode"
    );
    assert!(
        decoded.usage.output_tokens.is_some(),
        "the Messages API no longer reports output usage the adapter can decode"
    );
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
    }

    #[test]
    fn completed_with_zero_output_tokens_is_accepted() {
        // The adapter's own streamed fixtures already prove an `end_turn`
        // response with `output_tokens: Some(0)` decodes as `Completed`
        // (`stream.rs`), so this classifier must not reject it either —
        // only `assert_well_formed_response` requires usage merely present,
        // not positive, for exactly this reason.
        let expected_exchange = exchange(200);
        let expected_usage = TokenUsage {
            input_tokens: Some(3),
            output_tokens: Some(0),
            ..TokenUsage::default()
        };

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
    }

    #[test]
    fn downgraded_refusal_with_zero_output_tokens_is_accepted() {
        // A refusal can be blocked before any completion token is produced —
        // `output_tokens: Some(0)` is a valid, honest report here too (see
        // `completed_with_zero_output_tokens_is_accepted` above for the
        // completed path). The classifier accepts both shapes identically;
        // only `assert_well_formed_response`'s single, uniform
        // usage-presence check applies to either.
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
    fn unrecognized_200_provider_error_without_the_refusal_token_panics() {
        // Same `kind` and the same HTTP 200 status as the accepted
        // downgraded-refusal shape, but missing `without_unproven_refusal`'s
        // stable discriminator: a hypothetical future HTTP-200 Unrecognized
        // provider error reached some other way must not be waved through as
        // a refusal it never was.
        let _ = require_decoded_response(TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: exchange(200),
            reported_model: None,
            kind: ProviderErrorKind::Unrecognized,
            native: NativeErrorFacts::default(),
            usage: usage(),
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

/// Credential-free, straight-line coverage for `assert_well_formed_response`,
/// the helper the paid live test calls instead of branching on the decoded
/// shape itself.
#[cfg(test)]
mod assert_well_formed_response_tests {
    use super::*;

    /// The well-formed baseline every test perturbs by exactly one named
    /// field, per docs/agents/testing-style.md rule 4 — never three
    /// same-typed positional knobs a reader must cross-reference against a
    /// definition to tell apart.
    fn well_formed() -> DecodedResponse {
        DecodedResponse {
            exchange: ExchangeFacts {
                http_status: Some(200),
                ..ExchangeFacts::default()
            },
            usage: TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(1),
                ..TokenUsage::default()
            },
        }
    }

    #[test]
    fn positive_usage_passes() {
        assert_well_formed_response(&well_formed());
    }

    #[test]
    fn present_zero_output_tokens_passes() {
        // The exact edge both accept paths can legitimately report: a
        // present-but-zero output count is not a positivity requirement.
        let mut decoded = well_formed();
        decoded.usage.output_tokens = Some(0);
        assert_well_formed_response(&decoded);
    }

    #[test]
    #[should_panic(expected = "documented success status")]
    fn non_200_status_panics() {
        let mut decoded = well_formed();
        decoded.exchange.http_status = Some(500);
        assert_well_formed_response(&decoded);
    }

    #[test]
    #[should_panic(expected = "input usage")]
    fn missing_input_tokens_panics() {
        let mut decoded = well_formed();
        decoded.usage.input_tokens = None;
        assert_well_formed_response(&decoded);
    }

    #[test]
    #[should_panic(expected = "input usage")]
    fn zero_input_tokens_panics() {
        // Unlike output, input tokens must be positive: a request that
        // reached the model always billed at least one.
        let mut decoded = well_formed();
        decoded.usage.input_tokens = Some(0);
        assert_well_formed_response(&decoded);
    }

    #[test]
    #[should_panic(expected = "output usage")]
    fn missing_output_tokens_panics() {
        let mut decoded = well_formed();
        decoded.usage.output_tokens = None;
        assert_well_formed_response(&decoded);
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
        let expected_capability: u32 = 7;
        let outcome: PreparationOutcome<String, u32> =
            PreparationOutcome::Prepared(expected_capability);

        assert_eq!(require_prepared(outcome), expected_capability);
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
