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
    assert!(
        decoded.usage.output_tokens.is_some_and(|tokens| tokens > 0),
        "the Messages API no longer reports output usage the adapter can decode"
    );
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
/// `ProviderError { kind: Unrecognized, .. }` from the same HTTP 200 exchange
/// before returning (`without_unproven_refusal`, runtime.rs:716,729-742; the
/// "Refusal downgrade" rule in `docs/spec/runtime-substrate.md`). Matching the
/// dead `Refused` arm here would make this smoke fail a correctly decoded
/// refusal, so this recognizes what the adapter actually returns instead. The
/// `http_status == 200` guard keeps this arm from also swallowing a genuine
/// unrecognized 4xx/5xx provider error, which the assertions below must still
/// fail on.
#[track_caller]
fn require_decoded_response(evidence: TerminalEvidence) -> DecodedResponse {
    match evidence {
        TerminalEvidence::Completed(completed) => DecodedResponse {
            exchange: completed.exchange,
            usage: completed.usage,
        },
        TerminalEvidence::ProviderError(error)
            if error.kind == ProviderErrorKind::Unrecognized
                && error.exchange.http_status == Some(200) =>
        {
            DecodedResponse {
                exchange: error.exchange,
                usage: error.usage,
            }
        }
        // Adapter-produced evidence is already credential-shape redacted, so
        // printing it here cannot surface credential material.
        other => panic!("the Anthropic API returned no decoded response: {other:?}"),
    }
}
