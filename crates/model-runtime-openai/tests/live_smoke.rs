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
//! request the adapter builds, and its response still decodes through the
//! adapter's own types. Three outcomes pass, each carrying reported usage:
//!
//! - a completed outcome;
//! - the adapter's downgraded-refusal `ProviderError` shape (this transport
//!   exposes no independent proof that a response arrived only after the
//!   complete request was sent, so `OpenAiRuntime::execute` never returns a
//!   raw `Refused` — see `require_decoded_response` below);
//! - the exchange that stopped at this smoke's output ceiling, accepted
//!   because answer length is not this smoke's business. The decoder defers
//!   that verdict to `[DONE]` and refuses it outright if the requested final
//!   usage chunk never arrived, so this shape is held to the same usage bar
//!   as the other two.
//!
//! It deliberately asserts nothing about answer quality.
//!
//! This adapter is wired into signalboxd alongside the Anthropic adapter; this
//! smoke still validates the crate directly through its own `ModelRuntime`
//! implementation, not through the daemon composition root.
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

use std::time::Duration;

#[cfg(test)]
use signalbox_model_runtime::{
    BoundaryLossEvidence, CancellationConfirmedEvidence, CompletionEvidence, CompletionFinish,
    LossCause, PreparationDefect, PreparationFailure, ProvenUnsentEvidence, ProviderErrorEvidence,
    RefusalEvidence, UnsentCause,
};
use signalbox_model_runtime::{
    CancellationSignal, ConversationMessage, CredentialAccess, CredentialAccessError,
    CredentialAccessFailure, CredentialReference, CredentialValue, DeliveryMode, ExchangeFacts,
    FinishReason, ModelOperation, ModelRuntime, ModelSettings, NativeErrorFacts, Observation,
    ObservationFact, PreparationOutcome, ProviderErrorKind, ProviderReportedModel, RequestedTarget,
    ResolvedTarget, TerminalEvidence, TokenUsage, ToolCallsAtLoss,
};
use signalbox_model_runtime_openai::{
    OUTPUT_CEILING_VIOLATION_DETAIL, OpenAiConfig, OpenAiRuntime,
};

/// The environment variable this smoke reads its API key from. Configured in
/// CI via the `openai-smoke` environment; see
/// `.github/workflows/openai-smoke.yml`.
const API_KEY_VARIABLE: &str = "OPENAI_API_KEY";

/// The cheapest current OpenAI model, chosen so a compatibility run costs a
/// small fraction of a cent.
///
/// This is a reasoning model, so hidden reasoning tokens bill against the same
/// `max_completion_tokens` ceiling as visible output, and no wire control caps
/// them below it — `reasoning_effort` is a qualitative hint, not a token
/// budget. A run that spends the ceiling that way returns
/// `finish_reason: "length"`, which this adapter deliberately declines to
/// decode (`map_finish` in `src/response.rs`: OpenAI reuses `length` for both
/// the requested output ceiling and the model's context limit, and the adapter
/// will not guess which one occurred). That outcome is *accepted* rather than
/// designed away — see `require_decoded_response`, which recognizes the
/// ceiling shape explicitly — so this target's reasoning tier cannot redden a
/// required, twice-daily check.
const MODEL: &str = "gpt-5-nano";

/// A trivial prompt keeps the exchange to the smallest billable turn that
/// still exercises the whole response envelope.
const PROMPT: &str = "Reply with the single word: ready";

/// A generous ceiling for a one-word reply, kept as a cost cap. It is not the
/// determinism guarantee: `MODEL` reasons, so this ceiling also bounds hidden
/// reasoning tokens, and a run that exhausts it truncates. What keeps that
/// from reddening the check is `require_decoded_response` accepting the
/// resulting shape, not this number being large enough.
const MAX_OUTPUT_TOKENS: u32 = 512;

/// Bounds the one exchange well inside the workflow job's 10-minute budget.
/// A trivial one-word exchange healthy enough to prove compatibility completes
/// in seconds; two minutes leaves headroom in the workflow job while remaining
/// generous slack rather than a tight bound.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// Applies this smoke's timeout policy. The capability catalog stays empty: it
/// gates explicit provider
/// controls (reasoning effort, fast mode, service tier) against an exact-target
/// record, and this operation sets none of them, so an entry would only assert
/// a capability this smoke never exercises. In particular no reasoning effort
/// is pinned — this repository's own OpenAI catalog records that the
/// `"minimal"` effort "is listed by no current model page and appears on no
/// row" (`config/signalboxd.example.toml`), so pinning it would assert a
/// capability the repository does not claim.
fn openai_config() -> OpenAiConfig {
    let mut config = OpenAiConfig::new(None);
    config.exchange_timeout = Some(EXCHANGE_TIMEOUT);
    config
}

#[tokio::test]
#[ignore = "spends one real OpenAI exchange; run only from the gated compatibility smoke"]
async fn the_openai_api_completes_one_exchange() {
    let credential_reference = CredentialReference::new("openai-smoke");
    let runtime = OpenAiRuntime::new(
        openai_config(),
        EnvironmentCredential {
            variable: API_KEY_VARIABLE,
        },
    )
    .expect("smoke runtime configuration is valid");

    let settings = ModelSettings::new(MAX_OUTPUT_TOKENS);
    let mut operation = ModelOperation::new(
        "openai-smoke".to_string(),
        credential_reference,
        RequestedTarget::new(MODEL),
        ResolvedTarget::new(MODEL),
        vec![ConversationMessage::user_text(PROMPT)],
        settings,
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

/// `OpenAiPreparedRequest` deliberately implements no diagnostic formatting,
/// so each non-prepared outcome reports only its safe shared-runtime
/// evidence.
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
/// `OpenAiRuntime::execute` never returns a raw `TerminalEvidence::Refused` to
/// its caller. That downgrade is unconditional, so it covers this smoke's
/// streamed exchange too; the specification's refusal-downgrade rule states
/// why it is unconditional (a fully buffered HTTP request exposes no
/// independent proof that the response arrived only after the complete
/// upload, and the adapter fails toward known failure rather than inventing
/// evidence). `execute` therefore rewrites a decoded refusal into
/// `ProviderError { kind: Unrecognized, native: { error_token: None, .. }, .. }`
/// from the same HTTP 200 exchange before returning (`without_unproven_refusal`,
/// runtime.rs:616,628-646; the "Refusal downgrade" rule in
/// `docs/spec/runtime-substrate.md`). Matching the dead `Refused` arm here
/// would make this smoke fail a correctly decoded refusal, so this recognizes
/// what the adapter actually returns instead. The `http_status == 200` guard
/// keeps this arm from also swallowing a genuine unrecognized 4xx/5xx
/// provider error, which the assertions below must still fail on.
///
/// `kind` and outer status alone are not enough to identify that downgrade on
/// this transport, because a genuine failure can wear the same two facts: a
/// mid-stream `error` record inside an HTTP 200 SSE body is terminal
/// `ProviderError` evidence carrying that same 200 (`stream.rs`), and an error
/// whose native code and type are both unknown classifies as `Unrecognized`
/// (`classify_error_envelope` in `status.rs` falls through to the status, which
/// is zero for a record with no status of its own). Accepting on those two
/// facts would turn a real streamed provider failure green as long as usage
/// had accumulated. So this arm additionally requires two facts a native
/// stream error cannot produce:
///
/// - `native == NativeErrorFacts::default()`. The downgrade fabricates no
///   native material at all, while a native error record populates its facts
///   from the provider's own error object (`into_native_facts` in `wire.rs`
///   carries `type`, `code`, and `message` straight through).
/// - An observed `FinishReported(FinishReason::Refusal)`. The decoder emits
///   that fact only after the provider reports the refusal finish for the
///   choice; the native-error branch returns terminal evidence immediately,
///   emitting no finish at all.
///
/// Both are checked because either alone is defeatable: an error object with
/// every field null would clear the first, and the second cannot by itself
/// rule out a stream that reported a refusal finish and then failed natively.
///
/// A third shape is accepted: the exchange that stopped at this smoke's own
/// output ceiling. `PROMPT` asks for one word, but a prompt is a request, not
/// an enforced bound, and Chat Completions has no control that caps visible
/// output other than the ceiling itself. If the model ever runs to it, the
/// provider reports `finish_reason: "length"`, which `map_finish` leaves
/// `Unrecognized` on purpose (OpenAI reuses that token for both the requested
/// ceiling and the context limit, and the adapter will not guess), so the
/// decoder ends the stream as `BoundaryLoss` carrying the token verbatim.
/// That outcome is a truthful report about answer length, not a protocol
/// break: the request was accepted, the SSE body framed and decoded, and the
/// model identity and usage reported. It carries usage like the other two
/// accepted shapes, because the decoder defers this verdict to `[DONE]` and
/// refuses it outright if the requested final usage chunk never arrived, so
/// `assert_well_formed_response` holds all three to the same bar. Failing a
/// required, twice-daily paid check on it would be asserting something about
/// answer quality, which the owning specification says this smoke does not do.
/// The arm is keyed to that exact token from a 200 exchange that also reported
/// a model identity, so any other unrecognized finish, and every other loss
/// cause, still fails; a malformed envelope reaching a `length` finish carries
/// no reported finish at all (`stream.rs`) and so cannot reach this arm.
#[track_caller]
fn require_decoded_response(
    evidence: TerminalEvidence,
    observations: &[Observation<String>],
) -> DecodedResponse {
    match evidence {
        TerminalEvidence::Completed(completed) if !proposes_tool_calls(&completed.finish) => {
            DecodedResponse {
                exchange: completed.exchange,
                usage: completed.usage,
            }
        }
        TerminalEvidence::ProviderError(error)
            if is_the_refusal_downgrade_kind(error.kind)
                && error.exchange.http_status == Some(200)
                && error.native == NativeErrorFacts::default()
                && refusal_finish_observed(observations) =>
        {
            DecodedResponse {
                exchange: error.exchange,
                usage: error.usage,
            }
        }
        TerminalEvidence::BoundaryLoss(loss)
            if loss.exchange.http_status == Some(200)
                && loss.reported_model.is_some()
                && is_the_output_ceiling_violation(&loss)
                && stopped_at_the_output_ceiling(loss.finish_reported.as_ref()) =>
        {
            DecodedResponse {
                exchange: loss.exchange,
                usage: loss.usage,
            }
        }
        // Adapter-produced evidence is already credential-shape redacted, so
        // printing it here cannot surface credential material. Every variant
        // is named rather than caught by a wildcard, so a future
        // `TerminalEvidence` variant fails to compile here instead of
        // silently inheriting this panic path.
        rejected @ (TerminalEvidence::Completed(_)
        | TerminalEvidence::CompletedWithProviderCompaction { .. }
        | TerminalEvidence::Refused(_)
        | TerminalEvidence::ProviderError(_)
        | TerminalEvidence::CancellationConfirmed(_)
        | TerminalEvidence::ProvenUnsent(_)
        | TerminalEvidence::BoundaryLoss(_)) => {
            panic!("the OpenAI API returned no decoded response: {rejected:?}")
        }
    }
}

/// Whether this execution observed the provider reporting a refusal finish —
/// the corroboration `require_decoded_response` requires before accepting the
/// downgraded-refusal shape, and the one signal a mid-stream native error
/// record cannot manufacture.
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

/// The provider's own token for "generation stopped at the requested output
/// ceiling". Matched verbatim because the adapter deliberately does not map it
/// to a typed finish: OpenAI reuses `length` for the requested ceiling and the
/// context limit alike, so the adapter keeps the token rather than guessing
/// which bound was hit.
const OUTPUT_CEILING_FINISH_TOKEN: &str = "length";

/// Whether the stream ended because generation reached an output bound rather
/// than because the protocol broke.
///
/// `FinishReason` is enumerated rather than wildcarded: this helper gates a
/// merge-gating check, so a new finish variant must fail to compile here and be
/// considered instead of silently classifying as "not the ceiling".
fn stopped_at_the_output_ceiling(finish: Option<&FinishReason>) -> bool {
    match finish {
        Some(FinishReason::Unrecognized { provider_token }) => {
            provider_token == OUTPUT_CEILING_FINISH_TOKEN
        }
        Some(
            FinishReason::EndTurn
            | FinishReason::MaxOutputTokens
            | FinishReason::ContextWindowExceeded
            | FinishReason::StopSequence { .. }
            | FinishReason::ToolUse
            | FinishReason::Refusal,
        )
        | None => false,
    }
}

/// Whether a completion stopped to propose tool calls. This operation declares
/// no tools, so that finish means the provider volunteered a capability nobody
/// requested — the same anomaly the ceiling arm rejects on its own path, and
/// just as unacceptable when the response is otherwise well formed.
///
/// `CompletionFinish` is enumerated rather than compared, so a new finish
/// forces this acceptance policy to be reconsidered instead of defaulting to
/// "fine".
fn proposes_tool_calls(finish: &CompletionFinish) -> bool {
    match finish {
        CompletionFinish::ToolUse => true,
        CompletionFinish::EndTurn
        | CompletionFinish::MaxOutputTokens
        | CompletionFinish::ContextWindowExceeded
        | CompletionFinish::StopSequence { .. }
        | CompletionFinish::Unrecognized { .. } => false,
    }
}

/// Whether an error classification is the one the refusal downgrade produces.
///
/// `ProviderErrorKind` is enumerated rather than compared for equality: this
/// gates whether a provider error is accepted at all on a merge-gating check,
/// so a new classification must fail to compile here rather than silently
/// inherit the rejection path.
fn is_the_refusal_downgrade_kind(kind: ProviderErrorKind) -> bool {
    match kind {
        ProviderErrorKind::Unrecognized => true,
        ProviderErrorKind::CredentialRejected
        | ProviderErrorKind::PermissionDenied
        | ProviderErrorKind::InvalidRequest
        | ProviderErrorKind::TargetNotFound
        | ProviderErrorKind::RequestTooLarge
        | ProviderErrorKind::RateLimited
        | ProviderErrorKind::QuotaExhausted
        | ProviderErrorKind::Overloaded
        | ProviderErrorKind::ProviderInternal => false,
    }
}

/// Whether this loss is the plain output-ceiling stop rather than some other
/// protocol violation that reached the same evidence variant.
///
/// Two questions, answered separately. *Which* violation is this — still the
/// detail, because the loss vocabulary has no typed way to name the deferred
/// unrecognized-finish verdict; see `OUTPUT_CEILING_VIOLATION_DETAIL`. And *was
/// a tool call involved* — `tool_calls`.
///
/// Both are needed. Dropping the detail admits any stream defect that follows a
/// `length` finish before `[DONE]` — a record after the final usage chunk, a
/// conflicting completion id — since those reach identical typed evidence and
/// would pass this merge-gating check as a benign ceiling stop.
///
/// Only `NoneOpened` admits: `Unobserved` means the adapter could not establish
/// that no call opened, which is not a basis for passing a merge-gating check,
/// and `Opened` contradicts a plain ceiling stop outright. The two are collapsed
/// with an or-pattern rather than a wildcard, so they stay visibly rejected.
///
/// Both enums are enumerated rather than compared loosely, so a new `LossCause`
/// or a new `ToolCallsAtLoss` variant fails to compile here instead of silently
/// inheriting a verdict this merge gate never considered.
fn is_the_output_ceiling_violation(loss: &BoundaryLossEvidence) -> bool {
    let cause_admits = match &loss.cause {
        LossCause::StreamProtocolViolation { detail } => detail == OUTPUT_CEILING_VIOLATION_DETAIL,
        LossCause::CancellationRequested
        | LossCause::TimedOut(_)
        | LossCause::TransportFailed(_)
        | LossCause::ResponseBodyLost(_)
        | LossCause::ResponseUnintelligible { .. }
        | LossCause::UnexpectedHttpStatus
        | LossCause::StreamEndedWithoutTerminalMarker { .. } => false,
    };
    let tool_calls_admit = match loss.tool_calls {
        ToolCallsAtLoss::NoneOpened => true,
        ToolCallsAtLoss::Opened | ToolCallsAtLoss::Unobserved => false,
    };
    cause_admits && tool_calls_admit
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
///
/// One usage bar covers all three accepted shapes, including the
/// output-ceiling loss: the decoder defers that verdict to `[DONE]` so the
/// trailing usage-only chunk is consumed first, and refuses the verdict
/// outright if it never arrived. No shape is exempt.
///
/// Straight-line and credential-free: no test body branches on which accepted
/// shape arrived; the one branch lives here and has its own coverage below.
#[track_caller]
fn assert_well_formed_response(decoded: &DecodedResponse) {
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
        decoded.usage.output_tokens.is_some(),
        "the Chat Completions API no longer reports output usage the adapter can decode"
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
    /// native stream error never produces.
    fn refusal_observed() -> Vec<Observation<String>> {
        vec![Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::FinishReported(FinishReason::Refusal),
        }]
    }

    /// The native error facts a mid-stream `error` record carries through
    /// `into_native_facts`, as distinct from the downgrade's fabricated-free
    /// `NativeErrorFacts::default()`.
    fn native_stream_error_facts() -> NativeErrorFacts {
        NativeErrorFacts {
            error_token: Some("server_error".to_string()),
            error_code: None,
            message: Some("synthetic upstream failure".to_string()),
        }
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
        // Unrecognized` carried by the same HTTP 200 exchange, with no native
        // material at all (OpenAI's refusal comes from `finish_reason` or
        // `message.refusal`, not an error envelope), corroborated by the
        // refusal finish the decoder reported on the way there.
        let expected_exchange = exchange(200);
        let expected_usage = usage();

        let decoded = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: expected_exchange.clone(),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: NativeErrorFacts::default(),
                usage: expected_usage,
            }),
            &refusal_observed(),
        );

        assert_eq!(decoded.exchange, expected_exchange);
        assert_eq!(decoded.usage, expected_usage);
    }

    #[test]
    fn downgraded_refusal_with_zero_output_tokens_is_accepted() {
        // A content_filter refusal can be blocked before any completion
        // token is produced — `output_tokens: Some(0)` is a valid, honest
        // report here too (see `completed_with_zero_output_tokens_is_accepted`
        // above for the completed path). The classifier accepts both shapes
        // identically; only `assert_well_formed_response`'s single, uniform
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
                native: NativeErrorFacts::default(),
                usage: expected_usage,
            }),
            &refusal_observed(),
        );

        assert_eq!(decoded.exchange, expected_exchange);
        assert_eq!(decoded.usage, expected_usage);
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn native_stream_error_inside_a_200_body_panics() {
        // A mid-stream `error` record whose native code and type are both
        // unknown reaches the caller as `Unrecognized` carrying the outer
        // HTTP 200 and whatever usage had accumulated — the same two facts
        // the accepted downgrade shows. It is a genuine provider failure and
        // must stay red; the native material it carries is what separates it.
        let _ = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: exchange(200),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: native_stream_error_facts(),
                usage: usage(),
            }),
            &refusal_observed(),
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn unrefused_provider_error_without_native_material_panics() {
        // The residual case native facts alone cannot catch: an error record
        // carrying an empty error object would clear the native check. No
        // refusal finish was ever reported for it, so the observation check
        // still rejects it.
        let _ = require_decoded_response(
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: exchange(200),
                reported_model: None,
                kind: ProviderErrorKind::Unrecognized,
                non_acceptance_proven: false,
                native: NativeErrorFacts::default(),
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
                native: NativeErrorFacts::default(),
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
                native: NativeErrorFacts::default(),
                usage: TokenUsage::unreported(),
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
                // A status-only loss reads no response material.
                tool_calls: ToolCallsAtLoss::Unobserved,
                usage: TokenUsage::unreported(),
            }),
            &[],
        );
    }

    /// The well-formed output-ceiling loss every rejection case below perturbs
    /// by exactly one named field: what `StreamDecoder` produces when an
    /// otherwise healthy 200 stream reports `finish_reason: "length"` — the
    /// unrecognized-finish violation, carrying the token verbatim and the
    /// model identity the stream had already reported.
    ///
    /// Usage is present: deferring the verdict to `[DONE]` means the decoder
    /// consumes the trailing usage chunk first, and it now refuses the
    /// deferred verdict outright if that chunk never arrived.
    fn stopped_at_ceiling() -> BoundaryLossEvidence {
        BoundaryLossEvidence {
            cause: LossCause::StreamProtocolViolation {
                detail: OUTPUT_CEILING_VIOLATION_DETAIL.to_string(),
            },
            exchange: exchange(200),
            reported_model: Some(ProviderReportedModel::new("model-exact-1")),
            finish_reported: Some(FinishReason::Unrecognized {
                provider_token: OUTPUT_CEILING_FINISH_TOKEN.to_string(),
            }),
            // The stream is well formed up to the ceiling: every record
            // deserialized and was scanned for tool material, and none carried
            // any. The negative is a stated fact, not an absence.
            tool_calls: ToolCallsAtLoss::NoneOpened,
            usage: usage(),
        }
    }

    #[test]
    fn output_ceiling_truncation_is_accepted() {
        // A one-word prompt is a request, not an enforced bound. Running to
        // the ceiling still proves the protocol surface, so it must not redden
        // a required check.
        let expected = stopped_at_ceiling();

        let decoded =
            require_decoded_response(TerminalEvidence::BoundaryLoss(expected.clone()), &[]);

        assert_eq!(decoded.exchange, expected.exchange);
        assert_eq!(decoded.usage, expected.usage);
    }

    /// A different stream defect reaching the same typed evidence must not pass
    /// as a benign ceiling stop.
    ///
    /// A stream that reports `length` and then trips another defect before
    /// `[DONE]` keeps HTTP 200, the model identity, the retained `length`
    /// finish, and `NoneOpened` — every typed conjunct — so only the violation
    /// identity separates them. This is the merge-gating hole that dropping the
    /// detail comparison opens.
    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn another_violation_after_a_length_finish_panics() {
        let mut loss = stopped_at_ceiling();
        loss.cause = LossCause::StreamProtocolViolation {
            detail: "stream record follows the requested final usage chunk".to_string(),
        };

        let _ = require_decoded_response(TerminalEvidence::BoundaryLoss(loss), &[]);
    }

    /// A stop the adapter could not establish as tool-free is not a basis for
    /// passing a merge-gating check, so `Unobserved` is rejected alongside
    /// `Opened` rather than treated as "not opened".
    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn an_output_ceiling_stop_with_an_unobserved_tool_fact_panics() {
        let mut loss = stopped_at_ceiling();
        loss.tool_calls = ToolCallsAtLoss::Unobserved;

        let _ = require_decoded_response(TerminalEvidence::BoundaryLoss(loss), &[]);
    }

    #[test]
    fn an_accepted_output_ceiling_truncation_is_well_formed() {
        // The end-to-end guarantee the live test depends on: the classifier
        // and the assertion helper must agree, so this shape survives both
        // rather than only being accepted by the first.
        let decoded =
            require_decoded_response(TerminalEvidence::BoundaryLoss(stopped_at_ceiling()), &[]);

        assert_well_formed_response(&decoded);
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn a_different_unrecognized_finish_still_panics() {
        // Only the output-ceiling token is accepted: a finish token this
        // adapter has never seen is exactly the compatibility break the smoke
        // exists to catch.
        let mut loss = stopped_at_ceiling();
        loss.finish_reported = Some(FinishReason::Unrecognized {
            provider_token: "stop_and_smell_the_roses".to_string(),
        });

        let _ = require_decoded_response(TerminalEvidence::BoundaryLoss(loss), &[]);
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn an_output_ceiling_finish_from_a_non_200_status_panics() {
        let mut loss = stopped_at_ceiling();
        loss.exchange = exchange(500);

        let _ = require_decoded_response(TerminalEvidence::BoundaryLoss(loss), &[]);
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn an_unsolicited_tool_use_completion_panics() {
        // A well-formed completion is not automatically an acceptable one:
        // this operation declares no tools, so stopping to propose them means
        // the provider offered a capability nobody asked for.
        let _ = require_decoded_response(
            TerminalEvidence::Completed(CompletionEvidence {
                exchange: exchange(200),
                message_id: None,
                reported_model: None,
                finish: CompletionFinish::ToolUse,
                content: Vec::new(),
                usage: usage(),
            }),
            &[],
        );
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn an_output_ceiling_finish_after_a_tool_call_opened_panics() {
        // The decoder keeps `length` when a tool-bearing request truncates
        // mid-call, so the finish token cannot separate that from the benign
        // ceiling stop; `tool_calls` is what records the difference. This smoke
        // declares no tools, so an opened call means the provider volunteered
        // something nobody asked for.
        let mut loss = stopped_at_ceiling();
        loss.tool_calls = ToolCallsAtLoss::Opened;

        let _ = require_decoded_response(TerminalEvidence::BoundaryLoss(loss), &[]);
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn an_output_ceiling_finish_from_another_loss_cause_panics() {
        // Same status, model, and token, but the stream died some other way.
        let mut loss = stopped_at_ceiling();
        loss.cause = LossCause::UnexpectedHttpStatus;

        let _ = require_decoded_response(TerminalEvidence::BoundaryLoss(loss), &[]);
    }

    #[test]
    #[should_panic(expected = "returned no decoded response")]
    fn an_output_ceiling_finish_without_a_reported_model_panics() {
        // Synthetic: the decoder now rejects a missing model identity at the
        // unrecognized-finish chunk, before assigning the finish, so no
        // adapter-produced loss can reach this arm with both the ceiling
        // cause/token and no reported model. Kept as defense in depth — this
        // classifier must not start accepting that shape if the decoder's
        // ordering ever changes back.
        let mut loss = stopped_at_ceiling();
        loss.reported_model = None;

        let _ = require_decoded_response(TerminalEvidence::BoundaryLoss(loss), &[]);
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

/// Proves this smoke's own operation shape actually prepares, offline: the
/// live test spends a credentialed exchange, so a settings or configuration
/// mistake must fail here rather than there. `prepare` is where the adapter
/// validates settings against the target's capabilities, so an accidental
/// explicit provider control (which would demand an exact-target record this
/// config deliberately does not carry) fails this test immediately.
#[cfg(test)]
mod smoke_operation_tests {
    use super::*;

    #[derive(Debug)]
    struct FixedCredential;

    impl CredentialAccess for FixedCredential {
        async fn resolve(
            &self,
            _reference: &CredentialReference,
        ) -> Result<CredentialValue, CredentialAccessError> {
            Ok(CredentialValue::new(b"fixture-key".to_vec()))
        }
    }

    #[tokio::test]
    async fn the_smoke_operation_prepares_successfully() {
        let runtime = OpenAiRuntime::new(openai_config(), FixedCredential)
            .expect("smoke runtime configuration is valid");
        let mut operation = ModelOperation::new(
            "smoke-operation-test".to_string(),
            CredentialReference::new("openai-smoke"),
            RequestedTarget::new(MODEL),
            ResolvedTarget::new(MODEL),
            vec![ConversationMessage::user_text(PROMPT)],
            ModelSettings::new(MAX_OUTPUT_TOKENS),
        );
        operation.delivery = DeliveryMode::Streamed;

        let _ = require_prepared(
            runtime
                .prepare(operation, CancellationSignal::never())
                .await,
        );
    }
}
