//! Typed terminal evidence for one executed operation.
//!
//! Adapters report facts; the caller classifies them. Every variant is
//! structured so the caller can reach the model-call dispositions of
//! docs/spec/model-call-execution.md without inspecting any rendered string:
//! strings appear only as retained detail inside an already-classified
//! variant, never as the thing that decides the variant.

use std::time::{Duration, SystemTime};

use crate::message::AssistantPart;
use crate::target::ProviderReportedModel;
use crate::usage::TokenUsage;

/// The terminal report for one executed operation.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalReport<C> {
    /// The caller-supplied identity from the operation, returned verbatim.
    pub correlation: C,
    pub evidence: TerminalEvidence,
}

/// What provably happened to the one authorized provider interaction.
///
/// # Intended disposition mapping
///
/// This crate cannot import the domain's `ModelCallDisposition`; the caller
/// owns classification. The intended mapping, per the full-request-send rule
/// in docs/spec/model-call-execution.md:
///
/// - [`Completed`](Self::Completed): `Completed`.
/// - [`Refused`](Self::Refused): `Refused`.
/// - [`ProviderError`](Self::ProviderError): `KnownFailed`, for a complete,
///   correlated definitive provider error response; credential rejection
///   stays distinguishable via [`ProviderErrorKind::CredentialRejected`].
/// - [`CancellationConfirmed`](Self::CancellationConfirmed): `Cancelled`, for
///   a complete, correlated response definitively confirming provider
///   cancellation.
/// - [`ProvenUnsent`](Self::ProvenUnsent): `KnownFailed`, or `Cancelled` when
///   the cause is [`UnsentCause::CancelledBeforeSend`] and the caller holds
///   the applied-interrupt proof required by
///   docs/spec/model-call-execution.md.
/// - [`BoundaryLoss`](Self::BoundaryLoss): `Ambiguous` — the request crossed
///   or may have crossed the acceptance-capable boundary and no definitive
///   response classifies it.
///
/// A provider-reported model identity is carried as a separate fact where
/// observed; comparing it with the resolved target (the mismatch rule in
/// docs/spec/model-call-execution.md) is the caller's work.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvidence {
    /// A complete, correlated provider response with a terminal success
    /// status and valid completion material.
    Completed(CompletionEvidence),
    /// A completed response containing provider compaction, with the
    /// provider-reported retained input that remains relevant to the next
    /// request. This measure is distinct from the iteration-aggregated usage
    /// retained on `completion` for billing.
    CompletedWithProviderCompaction {
        completion: CompletionEvidence,
        retained_input_tokens: u64,
    },
    /// A complete exchange whose response reports the provider's refusal
    /// outcome rather than completion material.
    Refused(RefusalEvidence),
    /// A complete, correlated definitive provider error response.
    ProviderError(ProviderErrorEvidence),
    /// A complete, correlated provider response definitively confirming
    /// provider-side cancellation (the cancellation-response branch in
    /// docs/spec/model-call-execution.md). Neither in-repository adapter's
    /// provider documents such a response today; the variant keeps the
    /// vocabulary total so an adapter that observes one is never forced to
    /// misclassify it.
    CancellationConfirmed(CancellationConfirmedEvidence),
    /// The request provably never reached an acceptance-capable boundary.
    ProvenUnsent(ProvenUnsentEvidence),
    /// The request crossed or may have crossed the acceptance-capable
    /// boundary and the exchange ended without a definitive provider
    /// response.
    BoundaryLoss(BoundaryLossEvidence),
}

/// Correlated exchange facts observed at the provider boundary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExchangeFacts {
    /// The provider's request identifier (for the smoke-critical provider,
    /// the `request-id` response header), when observed.
    pub provider_request_id: Option<ProviderRequestId>,
    pub http_status: Option<u16>,
    /// Provider-directed minimum delay before another availability attempt.
    ///
    /// HTTP adapters decode `Retry-After`; process adapters populate this only
    /// when their typed protocol surface exposes an equivalent duration. The
    /// value contains no provider prose and is safe to persist as policy
    /// evidence.
    pub retry_after: Option<Duration>,
}

/// Decodes the HTTP `Retry-After` grammar into a delay from `now`.
///
/// Both the decimal delay-seconds form and the HTTP-date form are admitted.
/// A past date becomes zero delay; malformed or non-UTF-8 header material is
/// absent evidence rather than a guessed interval.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|deadline| deadline.duration_since(now).unwrap_or(Duration::ZERO))
}

/// A provider-issued request identifier, retained verbatim for support and
/// audit correlation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequestId(String);

impl ProviderRequestId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A provider-issued identifier for the response message itself, retained
/// verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMessageId(String);

impl ProviderMessageId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why the provider stopped generating, normalized to a closed vocabulary.
///
/// An unrecognized provider token is retained verbatim inside
/// [`Unrecognized`](Self::Unrecognized) so the caller never string-matches a
/// rendered message to learn it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// The model finished its turn.
    EndTurn,
    /// Generation hit the operation's output-token ceiling.
    MaxOutputTokens,
    /// Generation reached the model's context-window limit.
    ContextWindowExceeded,
    /// Generation hit a caller-declared stop sequence.
    StopSequence {
        /// The sequence the provider reported hitting, when reported.
        sequence: Option<String>,
    },
    /// The model stopped to propose tool calls.
    ToolUse,
    /// The provider reported a refusal outcome.
    Refusal,
    /// A stop reason this crate does not recognize, retained verbatim.
    Unrecognized {
        /// The provider's stop-reason token, exactly as observed.
        provider_token: String,
    },
}

impl FinishReason {
    /// This finish reason as a completion finish, or `None` for
    /// [`Refusal`](Self::Refusal): a refusal outcome is
    /// [`TerminalEvidence::Refused`], never completion.
    pub fn completion_finish(self) -> Option<CompletionFinish> {
        match self {
            Self::EndTurn => Some(CompletionFinish::EndTurn),
            Self::MaxOutputTokens => Some(CompletionFinish::MaxOutputTokens),
            Self::ContextWindowExceeded => Some(CompletionFinish::ContextWindowExceeded),
            Self::StopSequence { sequence } => Some(CompletionFinish::StopSequence { sequence }),
            Self::ToolUse => Some(CompletionFinish::ToolUse),
            Self::Refusal => None,
            Self::Unrecognized { provider_token } => {
                Some(CompletionFinish::Unrecognized { provider_token })
            }
        }
    }
}

/// Why a completed exchange stopped generating.
///
/// The refusal outcome is deliberately unrepresentable here: completion
/// evidence carrying a refusal stop reason would contradict
/// [`TerminalEvidence::Refused`], so the vocabulary excludes it by
/// construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionFinish {
    /// The model finished its turn.
    EndTurn,
    /// Generation hit the operation's output-token ceiling.
    MaxOutputTokens,
    /// Generation reached the model's context-window limit.
    ContextWindowExceeded,
    /// Generation hit a caller-declared stop sequence.
    StopSequence {
        /// The sequence the provider reported hitting, when reported.
        sequence: Option<String>,
    },
    /// The model stopped to propose tool calls.
    ToolUse,
    /// A stop reason this crate does not recognize, retained verbatim.
    Unrecognized {
        /// The provider's stop-reason token, exactly as observed.
        provider_token: String,
    },
}

impl From<CompletionFinish> for FinishReason {
    fn from(finish: CompletionFinish) -> Self {
        match finish {
            CompletionFinish::EndTurn => Self::EndTurn,
            CompletionFinish::MaxOutputTokens => Self::MaxOutputTokens,
            CompletionFinish::ContextWindowExceeded => Self::ContextWindowExceeded,
            CompletionFinish::StopSequence { sequence } => Self::StopSequence { sequence },
            CompletionFinish::ToolUse => Self::ToolUse,
            CompletionFinish::Unrecognized { provider_token } => {
                Self::Unrecognized { provider_token }
            }
        }
    }
}

/// Evidence for a completed exchange with valid completion material.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionEvidence {
    pub exchange: ExchangeFacts,
    pub message_id: Option<ProviderMessageId>,
    pub reported_model: Option<ProviderReportedModel>,
    pub finish: CompletionFinish,
    /// The assistant response parts, in provider order.
    pub content: Vec<AssistantPart>,
    pub usage: TokenUsage,
}

/// Evidence for a complete exchange the provider reported as refused.
#[derive(Debug, Clone, PartialEq)]
pub struct RefusalEvidence {
    pub exchange: ExchangeFacts,
    pub message_id: Option<ProviderMessageId>,
    pub reported_model: Option<ProviderReportedModel>,
    /// Any response parts produced before the refusal, in provider order.
    pub content: Vec<AssistantPart>,
    pub usage: TokenUsage,
}

/// Evidence for a complete, correlated definitive provider error response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderErrorEvidence {
    pub exchange: ExchangeFacts,
    /// The model identity the provider reported before or with the error,
    /// when observed — retained here so the mismatch precedence in
    /// docs/spec/model-call-execution.md can be applied from the
    /// authoritative terminal report alone.
    pub reported_model: Option<ProviderReportedModel>,
    /// The adapter's exhaustive classification of the provider's native
    /// error (docs/spec/runtime-substrate.md: each adapter owns an
    /// exhaustive, mutually exclusive native mapping).
    pub kind: ProviderErrorKind,
    /// Typed protocol proof that this error ended before any successful
    /// response stream could have been accepted.
    pub non_acceptance_proven: bool,
    /// The provider's native error material, retained verbatim as evidence.
    /// Classification never reads it.
    pub native: NativeErrorFacts,
    /// Provider-reported usage observed before or with the error.
    pub usage: TokenUsage,
}

/// The adapter's classification of a definitive provider error response.
///
/// Every kind maps to `KnownFailed` in docs/spec/model-call-execution.md;
/// the kinds exist so the caller can apply finer policy — the credential
/// boundary of docs/spec/runtime-substrate.md, rate-limit accounting —
/// without string inspection.
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum ProviderErrorKind {
    /// The provider rejected the request's credential
    /// (docs/spec/configuration-and-credentials.md: always known failure,
    /// with precedence over refusal).
    CredentialRejected,
    /// The credential is valid but not permitted this operation.
    PermissionDenied,
    /// The provider judged the request malformed or invalid.
    InvalidRequest,
    /// The provider does not recognize the requested resource or model.
    TargetNotFound,
    /// The request exceeded the provider's size limits.
    RequestTooLarge,
    /// The provider refused the request for rate-limit reasons.
    RateLimited,
    /// The provider reported the account's available quota exhausted — a
    /// billing condition, kept distinct from transient rate limiting so
    /// caller backoff policy never treats it as retry-later.
    QuotaExhausted,
    /// The provider reported itself overloaded.
    Overloaded,
    /// The provider reported an internal error.
    ProviderInternal,
    /// A definitive error response this adapter does not recognize; the
    /// native material is retained on the evidence.
    Unrecognized,
}

/// The provider's native error material, retained verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NativeErrorFacts {
    pub error_token: Option<String>,
    /// The provider's native error code, when the payload carried one
    /// distinct from the type token.
    pub error_code: Option<String>,
    pub message: Option<String>,
}

/// Evidence for a complete, correlated provider response that definitively
/// confirms provider-side cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancellationConfirmedEvidence {
    pub exchange: ExchangeFacts,
    /// The model identity reported by the definitive cancellation response,
    /// when present; retained for the target-mismatch precedence of
    /// docs/spec/model-call-execution.md.
    pub reported_model: Option<ProviderReportedModel>,
    /// The provider's native confirmation material, retained verbatim.
    pub native: NativeErrorFacts,
}

/// Evidence that the provider provably could not have accepted or acted on
/// the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenUnsentEvidence {
    pub cause: UnsentCause,
}

/// Why provider acceptance was provably impossible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsentCause {
    /// The caller's cancellation signal fired before any send was attempted.
    CancelledBeforeSend,
    /// Establishing the connection failed before any request byte could be
    /// written.
    ConnectFailed(TransportFacts),
    /// The request write began but did not complete, and the selected
    /// provider and transport contract proves partial input could not have
    /// been accepted or acted on (the incomplete-write proof of
    /// docs/spec/model-call-execution.md). The in-repository HTTP adapters
    /// never construct this: an HTTP server can begin acting before
    /// end-of-request framing, so their incomplete writes are boundary-loss
    /// evidence instead.
    SendIncompleteProvenUnacceptable(TransportFacts),
}

/// Evidence that the exchange ended without a definitive provider response
/// after the request crossed or may have crossed the acceptance-capable
/// boundary.
///
/// The intended classification for every cause, per
/// docs/spec/model-call-execution.md, is `Ambiguous`; the causes exist so
/// the caller and an operator can see *which* ambiguity occurred without
/// string inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryLossEvidence {
    pub cause: LossCause,
    /// Exchange facts observed before the loss, when any were.
    pub exchange: ExchangeFacts,
    /// The model identity the provider reported before the loss, when
    /// observed.
    pub reported_model: Option<ProviderReportedModel>,
    /// A finish reason reported before the loss, when observed. A reported
    /// refusal here is not refusal evidence: the exchange did not complete,
    /// so the completed-exchange precondition for `Refused` in
    /// docs/spec/model-call-execution.md is unmet.
    pub finish_reported: Option<FinishReason>,
    /// Whether a tool call had opened in the material decoded before the loss.
    pub tool_calls: ToolCallsAtLoss,
    /// Usage reported before the loss.
    pub usage: TokenUsage,
}

/// Whether a tool call had opened in the response material an adapter decoded
/// before the exchange was lost.
///
/// A tool call can open without producing any observation: a provider may
/// announce a call's identity and name and then be cut off before any argument
/// fragment, and the proposal observation is emitted only once the call is
/// finalized. The observation stream therefore cannot answer "had the provider
/// begun proposing tools when this was lost", and neither can [`LossCause`],
/// which answers only *how* the exchange was lost. This fact carries it, so a
/// caller reaches that distinction without reading a rendered
/// [`LossCause::StreamProtocolViolation`] detail.
///
/// Like [`finish_reported`](BoundaryLossEvidence::finish_reported) and
/// [`reported_model`](BoundaryLossEvidence::reported_model), this reports the
/// decoded prefix and nothing beyond it: [`NoneOpened`](Self::NoneOpened) says
/// no tool call opened in what the adapter decoded, never that the provider
/// sent none.
///
/// The variants are payload-free by design. An opened call's identity and
/// arguments already have a channel — `ToolCallProposed` and
/// `ToolArgumentsDelta` — wherever they were emitted at all, and a count adds
/// no classification power. What no other channel can carry is the bare fact
/// that a call had opened when nothing was emitted, which is what this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallsAtLoss {
    /// The adapter decoded the response's content material up to the loss and
    /// no tool call had opened in it.
    NoneOpened,
    /// At least one tool call had opened in the material decoded before the
    /// loss.
    Opened,
    /// The adapter's view of the response's tool material was incomplete when
    /// the loss occurred, so neither answer above is established.
    ///
    /// This is the absence of a *conclusion*, not the absence of decoded
    /// content: an adapter can reach it having decoded a great deal. It holds
    /// wherever material that could have opened a tool call went unexamined —
    /// a body that never parsed, a decode abandoned with content blocks still
    /// unread, a record or event whose payload failed to decode, material the
    /// runner read off the transport but never delivered to a decoder, and a
    /// loss raised by a layer that reads no response material at all. Never a
    /// claim that no tool call opened; `NoneOpened` is that claim, and it is
    /// made only where the adapter examined enough to support it.
    Unobserved,
}

/// How an exchange was lost after the request may have crossed the
/// acceptance-capable boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LossCause {
    /// The caller's cancellation signal fired after send commenced; the
    /// provider may still have accepted and processed the request.
    CancellationRequested,
    /// A local timeout elapsed with no definitive provider response.
    TimedOut(TransportFacts),
    /// Transport failure that cannot be proven to precede the
    /// acceptance-capable boundary.
    TransportFailed(TransportFacts),
    /// Response headers arrived but the response body was lost before it
    /// completed.
    ResponseBodyLost(TransportFacts),
    /// A complete success-status response body did not parse as the
    /// provider's completion material, so no definitive outcome exists.
    ResponseUnintelligible {
        /// The parser's rendered description.
        detail: String,
    },
    /// The response carried an HTTP status that is neither the provider's
    /// success nor error contract — a redirect, for example. Redirects are
    /// never followed (a follow could silently resend the request), so the
    /// status surfaces here as evidence.
    UnexpectedHttpStatus,
    /// The provider's event stream ended without the protocol's terminal
    /// marker: the explicit incomplete-stream fact, never silent success.
    StreamEndedWithoutTerminalMarker {
        /// How the stream ended.
        interruption: StreamInterruption,
    },
    /// The provider's event stream violated its protocol, so its contents
    /// cannot be trusted as an outcome.
    StreamProtocolViolation {
        /// What was violated.
        detail: String,
    },
}

/// How an event stream stopped without its terminal marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamInterruption {
    /// The stream ended cleanly at the transport level, but before the
    /// protocol's terminal marker.
    EndOfStream,
    /// The transport failed mid-stream.
    TransportFailure(TransportFacts),
    /// A caller-configured local deadline elapsed mid-stream, keeping the
    /// typed timeout cause visible inside the incomplete-stream fact
    /// (docs/spec/model-call-execution.md: a timeout after full send is
    /// ambiguous).
    TimedOut(TransportFacts),
}

/// Rendered transport detail, retained as evidence only.
///
/// Classification never depends on this text; it exists for operators and
/// audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportFacts {
    pub detail: String,
}

impl TransportFacts {
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::parse_retry_after;

    #[test]
    fn retry_after_accepts_delay_seconds_and_http_date() {
        let now = UNIX_EPOCH + Duration::from_secs(784_111_777);
        assert_eq!(parse_retry_after("23", now), Some(Duration::from_secs(23)));
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:57 GMT", now),
            Some(Duration::from_secs(20))
        );
    }

    #[test]
    fn retry_after_rejects_malformed_values_and_saturates_past_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(784_111_777);
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:00 GMT", now),
            Some(Duration::ZERO)
        );
    }
}
