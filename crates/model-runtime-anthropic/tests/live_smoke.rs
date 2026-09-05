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
//! Streamed delivery: the operation requests `DeliveryMode::Streamed`, which
//! is what `RuntimeModelCallProvider` in `crates/model-provider-runtime` sets
//! for ordinary model calls, generic over any adapter, so the one paid
//! exchange lands on the SSE decoder that carries production's main traffic.
//! It is not the *only* mode production selects: the approval-judge and
//! context-compaction callers in that same crate set `DeliveryMode::Buffered`,
//! and this smoke deliberately leaves that decoder to the adapter's offline
//! buffered fixtures rather than spending a second credentialed exchange on a
//! required, twice-daily check.
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
    LossCause, PreparationDefect, PreparationFailure, ProvenUnsentEvidence, ProviderErrorEvidence,
    RefusalEvidence, UnsentCause,
};
use std::time::Duration;

use signalbox_model_runtime::{
    CancellationSignal, ConversationMessage, CredentialAccess, CredentialAccessError,
    CredentialAccessFailure, CredentialReference, CredentialValue, DeliveryMode, ExchangeFacts,
    FinishReason, ModelOperation, ModelRuntime, ModelSettings, NativeErrorFacts, Observation,
    ObservationFact, PreparationOutcome, ProviderErrorKind, RequestedTarget, ResolvedTarget,
    TerminalEvidence, TokenUsage, ToolCallsAtLoss,
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
/// A trivial one-word exchange healthy enough to prove compatibility completes
/// in seconds; two minutes leaves headroom in the workflow job while remaining
/// generous slack rather than a tight bound.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// Applies this smoke's timeout policy.
fn anthropic_config() -> AnthropicConfig {
    let mut config = AnthropicConfig::new(None);
    config.exchange_timeout = Some(EXCHANGE_TIMEOUT);
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
    // The mode `RuntimeModelCallProvider` sets for ordinary model calls, so
    // the one paid exchange lands on the decoder carrying production's main
    // traffic. Production's buffered call sites are covered offline instead;
    // see the module doc comment.
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

    let decoded = require_decoded_response(report.evidence, &observations);
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
        PreparationOutcome::Cancelled { .. } => {
            panic!("smoke preparation was unexpectedly cancelled")
        }
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
/// to its caller. That downgrade is unconditional, so it covers this smoke's
/// streamed exchange too; the specification's refusal-downgrade rule states
/// why it is unconditional (a fully buffered HTTP request exposes no
/// independent proof that the response arrived only after the complete
/// upload, and the adapter fails toward known failure rather than inventing
/// evidence). `execute` therefore rewrites a decoded refusal into
/// `ProviderError { kind: Unrecognized, native: { error_token: Some("refusal"), .. }, .. }`
/// from the same HTTP 200 exchange before returning (`without_unproven_refusal`,
/// runtime.rs:716,729-742; the "Refusal downgrade" rule in
/// `docs/spec/runtime-substrate.md`). Matching the dead `Refused` arm here
/// would make this smoke fail a correctly decoded refusal, so this recognizes
/// what the adapter actually returns instead.
///
/// `kind` and outer status alone would not identify that downgrade, because a
/// genuine failure can wear both: a mid-stream `error` event inside an HTTP 200
/// SSE body is terminal `ProviderError` evidence carrying that same 200
/// (`stream.rs::apply_error`), and an error whose native type is not a
/// recognized token classifies as `Unrecognized`. The guard therefore requires
/// the whole shape `without_unproven_refusal` constructs and nothing else:
///
/// - `native == NativeErrorFacts { error_token: Some("refusal"), .. }` with no
///   code and no message. A native error event populates its facts from the
///   provider's own error object, which carries a rendered message.
/// - An observed `FinishReported(FinishReason::Refusal)`. The decoder emits
///   that only after the provider reports the refusal stop reason; the
///   error-event branch returns terminal evidence immediately, emitting no
///   finish at all.
///
/// Both are checked because either alone is defeatable: an error event whose
/// type happened to be the token `refusal` and which carried no message would
/// clear the first, and the second cannot by itself rule out a stream that
/// reported a refusal stop reason and then failed natively.
#[track_caller]
fn require_decoded_response(
    evidence: TerminalEvidence,
    observations: &[Observation<String>],
) -> DecodedResponse {
    match evidence {
        TerminalEvidence::Completed(completed) => DecodedResponse {
            exchange: completed.exchange,
            usage: completed.usage,
        },
        TerminalEvidence::ProviderError(error)
            if error.kind == ProviderErrorKind::Unrecognized
                && error.exchange.http_status == Some(200)
                && error.native == downgraded_refusal_facts()
                && refusal_finish_observed(observations) =>
        {
            DecodedResponse {
                exchange: error.exchange,
                usage: error.usage,
            }
        }
        // Adapter-produced evidence is already credential-shape redacted, so
        // printing it here cannot surface credential material. Every variant
        // is named rather than caught by a wildcard, so a future
        // `TerminalEvidence` variant fails to compile here instead of
        // silently inheriting this panic path.
        rejected @ (TerminalEvidence::CompletedWithProviderCompaction { .. }
        | TerminalEvidence::Refused(_)
        | TerminalEvidence::ProviderError(_)
        | TerminalEvidence::CancellationConfirmed(_)
        | TerminalEvidence::ProvenUnsent(_)
        | TerminalEvidence::BoundaryLoss(_)) => {
            panic!("the Anthropic API returned no decoded response: {rejected:?}")
        }
    }
}

/// The exact native facts `without_unproven_refusal` fabricates for the
/// downgrade: a stable discriminator token and nothing the provider actually
/// sent.
fn downgraded_refusal_facts() -> NativeErrorFacts {
    NativeErrorFacts {
        error_token: Some("refusal".to_string()),
        error_code: None,
        message: None,
    }
}

/// Whether this execution observed the provider reporting a refusal stop
/// reason — the corroboration `require_decoded_response` requires before
/// accepting the downgraded-refusal shape, and the one signal a mid-stream
/// native error event cannot manufacture.
fn refusal_finish_observed(observations: &[Observation<String>]) -> bool {
    observations
        .iter()
        .any(|observation| match &observation.fact {
            // Both owned enums are enumerated rather than wildcarded: this
            // discriminator decides whether a provider error is waved through
            // as a refusal, so a new observation class *or* a new finish
            // reason must fail to compile here and be considered, not
            // silently default to "no refusal".
            ObservationFact::FinishReported(finish) => match finish {
                FinishReason::Refusal => true,
                FinishReason::EndTurn
                | FinishReason::MaxOutputTokens
                | FinishReason::ContextWindowExceeded
                | FinishReason::StopSequence { .. }
                | FinishReason::ToolUse
                | FinishReason::Unrecognized { .. } => false,
            },
            ObservationFact::SendCommenced
            | ObservationFact::ExchangeEstablished(_)
            | ObservationFact::ProviderModelReported(_)
            | ObservationFact::TextDelta { .. }
            | ObservationFact::ThinkingDelta { .. }
            | ObservationFact::ToolArgumentsDelta { .. }
            | ObservationFact::ToolCallProposed(_)
            | ObservationFact::UsageReported(_) => false,
        })
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

    /// What the decoder emits before a refusal terminal: the corroboration a
    /// native error event never produces.
    fn refusal_observed() -> Vec<Observation<String>> {
        vec![Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::FinishReported(FinishReason::Refusal),
        }]
    }

    #[test]
    fn completed_evidence_is_accepted() {
        let expected_exchange = exchange(200);
        let expected_usage = usage();

        let decoded = require_decoded_response(
            TerminalEvidence::Completed(CompletionEvidence {
                exchange: expected_exchange.clone(),
                message_id: None,
                reported_model: None,
                finish: CompletionFinish::EndTurn,
                content: Vec::new(),
                usage: expected_usage,
            }),
            &[],
        );

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

        let decoded = require_decoded_response(
            TerminalEvidence::Completed(CompletionEvidence {
                exchange: expected_exchange.clone(),
                message_id: None,
                reported_model: None,
                finish: CompletionFinish::EndTurn,
                content: Vec::new(),
                usage: expected_usage,
            }),
            &[],
        );

        assert_eq!(decoded.exchange, expected_exchange);
        assert_eq!(decoded.usage, expected_usage);
    }

    #[test]
    fn downgraded_refusal_provider_error_is_accepted() {
        // The exact shape `without_unproven_refusal` constructs: `kind:
        // Unrecognized` carried by the same HTTP 200 exchange, with the
        // fabricated discriminator token and no provider material, plus the
        // refusal stop reason the decoder reported on the way there.
        let expected_exchange = exchange(200);
        let expected_usage = usage();

        let decoded = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: expected_exchange.clone(),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: downgraded_refusal_facts(),
                usage: expected_usage,
            }),
            &refusal_observed(),
        );

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

        let decoded = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: expected_exchange.clone(),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: downgraded_refusal_facts(),
                usage: expected_usage,
            }),
            &refusal_observed(),
        );

        assert_eq!(decoded.exchange, expected_exchange);
        assert_eq!(decoded.usage, expected_usage);
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn native_error_event_inside_a_200_body_panics() {
        // A mid-stream `error` event reaches the caller as terminal provider
        // evidence carrying the outer HTTP 200 and whatever usage had
        // accumulated. Even with the same `kind` and a type token that
        // happened to read `refusal`, the provider's rendered message is
        // material the downgrade never fabricates, so this genuine failure
        // stays red.
        let _ = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: exchange(200),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: NativeErrorFacts {
                    error_token: Some("refusal".to_string()),
                    error_code: None,
                    message: Some("synthetic upstream failure".to_string()),
                },
                usage: usage(),
            }),
            &refusal_observed(),
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn downgraded_refusal_shape_without_an_observed_refusal_panics() {
        // The residual case native facts alone cannot catch: an error event
        // carrying only the token `refusal` and nothing else would clear the
        // native check. No refusal stop reason was ever reported for it, so
        // the observation check still rejects it.
        let _ = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: exchange(200),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: downgraded_refusal_facts(),
                usage: usage(),
            }),
            &[],
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn an_ordinary_observation_is_not_mistaken_for_a_refusal() {
        // The discriminator's negative path, exercised with real observations
        // rather than an empty slice: a stream that reported usage and an
        // end-turn finish never reported a refusal, so the downgrade-shaped
        // provider error must still fail.
        let observations = vec![
            Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::UsageReported(usage()),
            },
            Observation {
                correlation: "call-1".to_string(),
                fact: ObservationFact::FinishReported(FinishReason::EndTurn),
            },
        ];

        let _ = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: exchange(200),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: downgraded_refusal_facts(),
                usage: usage(),
            }),
            &observations,
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn raw_refused_panics() {
        let _ = require_decoded_response(
            TerminalEvidence::Refused(RefusalEvidence {
                exchange: exchange(200),
                message_id: None,
                reported_model: None,
                content: Vec::new(),
                usage: usage(),
            }),
            &refusal_observed(),
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn unrecognized_provider_error_from_a_non_200_status_panics() {
        // Same `kind` as the accepted downgraded-refusal shape, but from a
        // real error status: the `http_status == 200` guard must keep this
        // from being swallowed as a well-formed decode.
        let _ = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: exchange(500),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: downgraded_refusal_facts(),
                usage: TokenUsage::unreported(),
            }),
            &refusal_observed(),
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn unrecognized_200_provider_error_without_the_refusal_token_panics() {
        // Same `kind` and the same HTTP 200 status as the accepted
        // downgraded-refusal shape, but missing `without_unproven_refusal`'s
        // stable discriminator: a hypothetical future HTTP-200 Unrecognized
        // provider error reached some other way must not be waved through as
        // a refusal it never was.
        let _ = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: exchange(200),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: NativeErrorFacts::default(),
                usage: usage(),
            }),
            &refusal_observed(),
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn recognized_provider_error_panics() {
        let _ = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: exchange(401),
                reported_model: None,
                kind: ProviderErrorKind::CredentialRejected,
                non_acceptance_proven: false,
                native: NativeErrorFacts::default(),
                usage: TokenUsage::unreported(),
            }),
            &refusal_observed(),
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn cancellation_confirmed_panics() {
        let _ = require_decoded_response(
            TerminalEvidence::CancellationConfirmed(CancellationConfirmedEvidence {
                exchange: exchange(200),
                reported_model: None,
                native: NativeErrorFacts::default(),
            }),
            &[],
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn proven_unsent_panics() {
        let _ = require_decoded_response(
            TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                cause: UnsentCause::CancelledBeforeSend,
            }),
            &[],
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn boundary_loss_panics() {
        let _ = require_decoded_response(
            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                cause: LossCause::UnexpectedHttpStatus,
                exchange: exchange(200),
                reported_model: None,
                finish_reported: None,
                tool_calls: ToolCallsAtLoss::Unobserved,
                usage: TokenUsage::unreported(),
            }),
            &[],
        );
    }
}

/// Credential-free, straight-line coverage for `assert_well_formed_response`,
/// the helper the paid live test calls instead of branching on the decoded
/// shape itself.
#[cfg(test)]
mod assert_well_formed_response_tests {
    use super::*;

    /// The well-formed baseline every test perturbs by exactly one named
    /// field — never three same-typed positional knobs a reader must
    /// cross-reference against a definition to tell apart.
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
    #[should_panic(expected = "smoke preparation was unexpectedly cancelled")]
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
