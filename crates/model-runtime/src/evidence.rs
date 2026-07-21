//! Typed terminal evidence for one executed operation.
//!
//! Adapters report facts; the caller classifies them. Every variant is
//! structured so the caller can reach ADR-0043's model-call dispositions
//! without inspecting any rendered string: strings appear only as retained
//! detail inside an already-classified variant, never as the thing that
//! decides the variant.

use crate::message::AssistantPart;
use crate::target::ProviderReportedModel;
use crate::usage::TokenUsage;

/// The terminal report for one executed operation: the caller's correlation
/// identity plus the evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalReport<C> {
    /// The caller-supplied identity from the operation, returned verbatim.
    pub correlation: C,
    /// What provably happened.
    pub evidence: TerminalEvidence,
}

/// What provably happened to the one authorized provider interaction.
///
/// # Intended ADR-0043 mapping
///
/// This crate cannot import the domain's `ModelCallDisposition`; the caller
/// owns classification. The intended mapping, per ADR-0043's
/// full-request-send rule:
///
/// | Evidence | Intended disposition |
/// |---|---|
/// | [`Completed`](Self::Completed) | `Completed` |
/// | [`Refused`](Self::Refused) | `Refused` |
/// | [`ProviderError`](Self::ProviderError) | `KnownFailed` (a complete, correlated definitive provider error response; credential rejection stays distinguishable via [`ProviderErrorKind::CredentialRejected`]) |
/// | [`ProvenUnsent`](Self::ProvenUnsent) | `KnownFailed`, or `Cancelled` when the cause is [`UnsentCause::CancelledBeforeSend`] and the caller holds ADR-0005's applied-interrupt proof |
/// | [`BoundaryLoss`](Self::BoundaryLoss) | `Ambiguous` — the request crossed or may have crossed the acceptance-capable boundary and no definitive response classifies it |
///
/// A provider-reported model identity is carried as a separate fact where
/// observed; comparing it with the resolved target (ADR-0005's mismatch
/// rule) is the caller's work.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvidence {
    /// A complete, correlated provider response with a terminal success
    /// status and valid completion material.
    Completed(CompletionEvidence),
    /// A complete exchange whose response reports the provider's refusal
    /// outcome rather than completion material.
    Refused(RefusalEvidence),
    /// A complete, correlated definitive provider error response.
    ProviderError(ProviderErrorEvidence),
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
    /// The HTTP status of the response, when the exchange produced one.
    pub http_status: Option<u16>,
}

/// A provider-issued request identifier, retained verbatim for support and
/// audit correlation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequestId(String);

impl ProviderRequestId {
    /// Wraps a provider request identifier exactly as observed.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The identifier as observed.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A provider-issued identifier for the response message itself, retained
/// verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMessageId(String);

impl ProviderMessageId {
    /// Wraps a provider message identifier exactly as observed.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The identifier as observed.
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

/// Evidence for a completed exchange with valid completion material.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionEvidence {
    /// Correlated exchange facts.
    pub exchange: ExchangeFacts,
    /// The provider's identifier for the response message, when reported.
    pub message_id: Option<ProviderMessageId>,
    /// The model identity the provider reported, when reported. Comparing it
    /// with the resolved target is the caller's classification work.
    pub reported_model: Option<ProviderReportedModel>,
    /// Why generation stopped. Never [`FinishReason::Refusal`]: a refusal
    /// outcome is reported as [`TerminalEvidence::Refused`].
    pub finish: FinishReason,
    /// The assistant response parts, in provider order.
    pub content: Vec<AssistantPart>,
    /// Provider-reported usage.
    pub usage: TokenUsage,
}

/// Evidence for a complete exchange the provider reported as refused.
#[derive(Debug, Clone, PartialEq)]
pub struct RefusalEvidence {
    /// Correlated exchange facts.
    pub exchange: ExchangeFacts,
    /// The provider's identifier for the response message, when reported.
    pub message_id: Option<ProviderMessageId>,
    /// The model identity the provider reported, when reported.
    pub reported_model: Option<ProviderReportedModel>,
    /// Any response parts produced before the refusal, in provider order.
    pub content: Vec<AssistantPart>,
    /// Provider-reported usage.
    pub usage: TokenUsage,
}

/// Evidence for a complete, correlated definitive provider error response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderErrorEvidence {
    /// Correlated exchange facts.
    pub exchange: ExchangeFacts,
    /// The adapter's exhaustive classification of the provider's native
    /// error (ADR-0043: each adapter owns an exhaustive, mutually exclusive
    /// native mapping).
    pub kind: ProviderErrorKind,
    /// The provider's native error material, retained verbatim as evidence.
    /// Classification never reads it.
    pub native: NativeErrorFacts,
}

/// The adapter's classification of a definitive provider error response.
///
/// Every kind maps to ADR-0043 `KnownFailed`; the kinds exist so the caller
/// can apply finer policy — ADR-0017's credential boundary, rate-limit
/// accounting — without string inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// The provider rejected the request's credential (ADR-0017: always
    /// known failure, with precedence over refusal).
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
    /// The provider's native error-type token, when the payload carried one.
    pub error_token: Option<String>,
    /// The provider's rendered error message, when the payload carried one.
    pub message: Option<String>,
}

/// Evidence that the request provably never reached an acceptance-capable
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenUnsentEvidence {
    /// Why the request was provably never sent.
    pub cause: UnsentCause,
}

/// Why a request was provably never sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsentCause {
    /// Local preparation failed before any send was attempted.
    PreparationFailed(PreparationFailure),
    /// The caller's cancellation signal fired before any send was attempted.
    CancelledBeforeSend,
    /// Establishing the connection failed before any request byte could be
    /// written.
    ConnectFailed(TransportFacts),
}

/// A local preparation failure, classified before any transport work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparationFailure {
    /// The operation asks for something this adapter does not support.
    UnsupportedOperation {
        /// What the adapter does not support.
        detail: String,
    },
    /// The operation could not be serialized into the provider's wire shape.
    SerializationFailed {
        /// The serializer's rendered description.
        detail: String,
    },
    /// The adapter's configuration cannot address the provider.
    InvalidConfiguration {
        /// What is invalid.
        detail: String,
    },
}

/// Evidence that the exchange ended without a definitive provider response
/// after the request crossed or may have crossed the acceptance-capable
/// boundary.
///
/// The intended ADR-0043 classification for every cause is `Ambiguous`; the
/// causes exist so the caller and an operator can see *which* ambiguity
/// occurred without string inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryLossEvidence {
    /// How the exchange was lost.
    pub cause: LossCause,
    /// Exchange facts observed before the loss, when any were.
    pub exchange: ExchangeFacts,
    /// The model identity the provider reported before the loss, when
    /// observed.
    pub reported_model: Option<ProviderReportedModel>,
    /// A finish reason reported before the loss, when observed. A reported
    /// refusal here is not refusal evidence: the exchange did not complete,
    /// so ADR-0043's completed-exchange precondition for `Refused` is unmet.
    pub finish_reported: Option<FinishReason>,
    /// Usage reported before the loss.
    pub usage: TokenUsage,
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
}

/// Rendered transport detail, retained as evidence only.
///
/// Classification never depends on this text; it exists for operators and
/// audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportFacts {
    /// The transport's rendered description of what happened.
    pub detail: String,
}

impl TransportFacts {
    /// Wraps rendered transport detail.
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}
