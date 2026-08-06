//! Compatibility smoke against the real OpenAI Chat Completions API.
//!
//! Ignored by default: it spends one real model exchange and therefore needs
//! credentials the ordinary Rust workflow never has.
//! `.github/workflows/openai-smoke.yml` is the only automated caller: its
//! unprivileged gate rejects changed fork pull requests before the
//! environment-backed smoke job can start.
//!
//! What it proves is protocol compatibility, which is what a public API
//! change actually breaks: `POST /v1/chat/completions` still accepts the
//! request the adapter builds, and its response still decodes as a completed
//! or refused terminal outcome through the adapter's own types, with usage
//! reported. It deliberately asserts nothing about answer quality.
//!
//! This adapter is not yet wired into signalboxd (unlike the Anthropic
//! adapter); this smoke validates the crate itself, in isolation.
//!
//! No prompt caching: this smoke sends one small, fixed prompt and nothing
//! else. At that volume a cache write costs more than it could ever recoup,
//! so the request deliberately carries no cache-control breakpoints.
//!
//! Credential discipline: this test never reads, prints, or logs credential
//! material. It resolves `OPENAI_API_KEY` from the environment through the
//! same [`signalbox_model_runtime::CredentialAccess`] boundary production
//! code uses, and the adapter's own redaction sanitizes any
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
    ModelRuntime, ModelSettings, PreparationOutcome, RequestedTarget, ResolvedTarget,
    TerminalEvidence, TokenUsage,
};
use signalbox_model_runtime_openai::{OpenAiConfig, OpenAiRuntime};

/// The environment variable this smoke reads its API key from. Owner-managed
/// in CI via the `openai-smoke` environment; see
/// `.github/workflows/openai-smoke.yml`.
const API_KEY_VARIABLE: &str = "OPENAI_API_KEY";

/// The cheapest current OpenAI model, chosen so a compatibility run costs a
/// small fraction of a cent.
const MODEL: &str = "gpt-5-nano";

/// A trivial prompt keeps the exchange to the smallest billable turn that
/// still exercises the whole response envelope.
const PROMPT: &str = "Reply with the single word: ready";

#[tokio::test]
#[ignore = "spends one real OpenAI exchange; run only from the gated compatibility smoke"]
async fn the_openai_api_completes_one_exchange() {
    let credential_reference = CredentialReference::new("openai-smoke");
    let runtime = OpenAiRuntime::new(
        OpenAiConfig::new(),
        EnvironmentCredential {
            variable: API_KEY_VARIABLE,
        },
    )
    .expect("smoke runtime configuration is valid");

    let operation = ModelOperation::new(
        "openai-smoke".to_string(),
        credential_reference,
        RequestedTarget::new(MODEL),
        ResolvedTarget::new(MODEL),
        vec![ConversationMessage::user_text(PROMPT)],
        ModelSettings::new(64),
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
        "the Chat Completions API no longer reports input usage the adapter can decode"
    );
    assert!(
        decoded.usage.output_tokens.is_some_and(|tokens| tokens > 0),
        "the Chat Completions API no longer reports output usage the adapter can decode"
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

/// `OpenAiPreparedRequest` deliberately implements no diagnostic formatting,
/// so each non-prepared outcome reports only its safe shared-runtime
/// evidence.
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

/// Accepts either a completion or a refusal: both are well-formed, decoded
/// evidence proving the adapter's response contract still holds. Only a
/// terminal outcome the adapter never decoded (a transport, protocol, or
/// provider-error class) fails this gate.
#[track_caller]
fn require_decoded_response(evidence: TerminalEvidence) -> DecodedResponse {
    match evidence {
        TerminalEvidence::Completed(completed) => DecodedResponse {
            exchange: completed.exchange,
            usage: completed.usage,
        },
        TerminalEvidence::Refused(refused) => DecodedResponse {
            exchange: refused.exchange,
            usage: refused.usage,
        },
        // Adapter-produced evidence is already credential-shape redacted, so
        // printing it here cannot surface credential material.
        other => panic!("the OpenAI API returned no decoded response: {other:?}"),
    }
}
