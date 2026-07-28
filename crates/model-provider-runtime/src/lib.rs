//! Bridge from the application-owned model-call port to a Layer-1 runtime.
//!
//! The layer boundary in docs/spec/runtime-substrate.md keeps runtime types
//! out of the domain and application crates. This crate is the outward
//! adapter: it translates one checked application operation, moves the
//! runtime's opaque one-shot capability across durable authorization, and
//! maps typed terminal evidence into the domain dispositions defined in
//! docs/spec/model-call-execution.md. It owns no retry, fallback, lifecycle,
//! or durable state.

mod context_compaction;

pub use context_compaction::{
    ContextCompactionModel, ContextCompactionModelError, ContextCompactionModelRequest,
    ContextCompactionModelResult, RuntimeContextCompactionModel,
};

use std::{collections::HashMap, error::Error, fmt, future::Future, sync::Arc};

use signalbox_application::{
    ClassifyOperatorFailure, ModelCallCapabilityPreparation, ModelCallInputTokenCount,
    ModelCallInputTokenCounter, ModelCallProvider, ModelConversationMessage,
    ModelToolResultContent, OperatorFailureClass, PreparedModelOperation,
};
use signalbox_domain::{
    AssistantResponsePart, AssistantText, AuthorizedModelCall, ContextFrontierId,
    FrozenModelSelection, ModelCallId, ModelCallTerminalObservation, NormalizedToolArguments,
    ResolvedProviderTarget, SessionId, ToolArgumentsKind,
    ToolCallProposal as DomainToolCallProposal, ToolExecutionErrorKind, ToolName as DomainToolName,
    ToolResultContent, ToolUsingAssistantResponse, TurnAttemptId, TurnId,
};
use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, ConversationMessage, ConversationRole,
    CredentialAccessFailure, CredentialReference, DeliveryMode, LossCause, MessagePart,
    ModelOperation, ModelRuntime, ModelSettings, Observation, ObservationFact, ObservationSink,
    PreparationFailure, PreparationOutcome, ProviderErrorKind, ProviderReportedModel,
    RequestedTarget, ResolvedTarget, TerminalEvidence, ToolCallId, ToolCallProposal,
    ToolDefinition, ToolName as RuntimeToolName, ToolResultRecord, UnsentCause,
};

/// The longest provider-reported model identity retained for operator
/// diagnostics.
///
/// The provider controls the reported spelling, so the diagnostic projection
/// is bounded before it can reach a log line.
const DIAGNOSTIC_MODEL_IDENTITY_LIMIT: usize = 128;

const MODEL_IDENTITY_CHANGE_MESSAGE: &str = "Signalbox session event: your model identity is now";
const CONTEXT_SUMMARY_MESSAGE: &str = "Signalbox prior-conversation summary:";

/// One already-redacted provider text fragment for ephemeral presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTextDelta {
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    part_index: u32,
    text: Arc<str>,
}

impl ProviderTextDelta {
    fn new(
        session: SessionId,
        turn: TurnId,
        call: ModelCallId,
        part_index: u32,
        text: String,
    ) -> Self {
        Self {
            session,
            turn,
            call,
            part_index,
            text: text.into(),
        }
    }

    /// Returns the session whose active turn produced this fragment.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the active turn that produced this fragment.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the correlated model call that produced this fragment.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the provider part position this fragment extends.
    pub const fn part_index(&self) -> u32 {
        self.part_index
    }

    /// Borrows the already-redacted provider text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Best-effort presentation sink for already-redacted provider text deltas.
///
/// Delivery is additive and has no effect on terminal evidence collection or
/// classification. Implementations must not block model execution.
pub trait ProviderTextDeltaSink: Send + Sync {
    /// Offers one correlated fragment for ephemeral presentation.
    fn publish(&self, delta: ProviderTextDelta);
}

#[derive(Debug)]
struct DiscardProviderTextDeltas;

impl ProviderTextDeltaSink for DiscardProviderTextDeltas {
    fn publish(&self, _delta: ProviderTextDelta) {}
}

/// One exact provider-model spelling and baseline request limit for a durable
/// domain target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeModelDefinition {
    target: ResolvedProviderTarget,
    provider_model: String,
    max_output_tokens: u32,
    context_window_tokens: u32,
}

impl RuntimeModelDefinition {
    /// Associates a durable target with one exact provider model spelling.
    pub fn try_new(
        target: ResolvedProviderTarget,
        provider_model: String,
        max_output_tokens: u32,
        context_window_tokens: u32,
    ) -> Result<Self, RuntimeModelDefinitionError> {
        if provider_model.is_empty() || provider_model.trim() != provider_model {
            return Err(RuntimeModelDefinitionError::InvalidProviderModel);
        }
        if max_output_tokens == 0 {
            return Err(RuntimeModelDefinitionError::InvalidOutputLimit);
        }
        if context_window_tokens == 0 {
            return Err(RuntimeModelDefinitionError::InvalidContextWindow);
        }
        Ok(Self {
            target,
            provider_model,
            max_output_tokens,
            context_window_tokens,
        })
    }

    /// Returns the durable exact target represented by this mapping.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }

    /// Returns the exact provider-native model spelling.
    pub fn provider_model(&self) -> &str {
        &self.provider_model
    }

    /// Returns the required provider output-token ceiling.
    pub const fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    /// Returns the operator-declared input context-window limit.
    pub const fn context_window_tokens(&self) -> u32 {
        self.context_window_tokens
    }
}

/// A runtime delivery definition cannot construct a request-safe mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeModelDefinitionError {
    /// The provider model spelling was empty or padded.
    InvalidProviderModel,
    /// A provider request requires a positive output-token ceiling.
    InvalidOutputLimit,
    /// Automatic guarding requires a positive declared context window.
    InvalidContextWindow,
}

impl fmt::Display for RuntimeModelDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidProviderModel => "provider model spelling is empty or padded",
            Self::InvalidOutputLimit => "provider output-token limit is zero",
            Self::InvalidContextWindow => "provider context-window limit is zero",
        })
    }
}

impl Error for RuntimeModelDefinitionError {}

/// Immutable runtime delivery mappings indexed by durable exact target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeModelCatalog {
    definitions: HashMap<ResolvedProviderTarget, RuntimeModelDefinition>,
}

impl RuntimeModelCatalog {
    /// Builds a catalog and rejects conflicting meanings for one target.
    pub fn try_from_definitions(
        definitions: impl IntoIterator<Item = RuntimeModelDefinition>,
    ) -> Result<Self, RuntimeModelCatalogError> {
        let mut by_target = HashMap::new();
        for definition in definitions {
            if let Some(existing) = by_target.get(&definition.target)
                && existing != &definition
            {
                return Err(RuntimeModelCatalogError::ConflictingTarget {
                    target: definition.target,
                });
            }
            by_target.insert(definition.target, definition);
        }
        Ok(Self {
            definitions: by_target,
        })
    }

    /// Looks up the exact runtime delivery mapping for a durable target.
    pub fn resolve(&self, target: ResolvedProviderTarget) -> Option<&RuntimeModelDefinition> {
        self.definitions.get(&target)
    }
}

/// Two deployment definitions assigned conflicting meanings to one target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeModelCatalogError {
    /// One target named distinct provider spellings or output limits.
    ConflictingTarget {
        /// The target whose immutable meaning conflicted.
        target: ResolvedProviderTarget,
    },
}

impl fmt::Display for RuntimeModelCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("runtime model catalog contains a conflicting target")
    }
}

impl Error for RuntimeModelCatalogError {}

/// How one provider-reported model identity relates to the configured exact
/// provider-model spelling for the call's resolved target.
///
/// docs/spec/model-call-execution.md owns the law this vocabulary encodes:
/// an alias made concrete is the same logical target, while a served identity
/// from another lineage is a substitution the daemon never authorized.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProviderTargetRelation {
    /// The reported identity is byte-identical to the configured spelling.
    Exact,
    /// The reported identity is the configured spelling made concrete by a
    /// provider dated-snapshot qualifier — the same logical target, named in
    /// its canonical concrete form.
    AliasConcretion,
    /// The reported identity names a different model lineage: the provider
    /// served something other than the configured target.
    DifferentLineage,
}

/// Relates one provider-reported identity to the configured exact spelling.
///
/// The rule is derived from the configured target's own family, never from a
/// table of known provider identifiers:
///
/// - equal spellings are [`Exact`](ProviderTargetRelation::Exact);
/// - the configured spelling followed by `-` and a *dated snapshot qualifier*
///   is [`AliasConcretion`](ProviderTargetRelation::AliasConcretion) — the
///   configured family made concrete;
/// - everything else is
///   [`DifferentLineage`](ProviderTargetRelation::DifferentLineage).
///
/// A dated snapshot qualifier is `YYYYMMDD` or `YYYY-MM-DD`; calendar
/// validity is deliberately not checked, because the shape alone makes the
/// distinction. Requiring a full date shape — rather than
/// any trailing segment — is what keeps a *version* extension of the same
/// family name from being read as a snapshot: with `claude-opus-4`
/// configured, `claude-opus-4-5` extends the family by one digit and stays
/// `DifferentLineage`, while `claude-haiku-4-5-20251001` against a configured
/// `claude-haiku-4-5` is the same lineage made concrete. Any other extension —
/// a delivery or speed variant, a differently named family — is likewise a
/// different lineage, because it is not the configured target.
/// The two arguments are the distinct target facts of
/// docs/spec/runtime-substrate.md rather than two adjacent strings, so a
/// caller cannot silently transpose this authorization-sensitive comparison:
/// the relation is asymmetric, and swapping the operands would turn an alias
/// concretion into a substitution.
pub fn relate_provider_target(
    configured: &ResolvedTarget,
    reported: &ProviderReportedModel,
) -> ProviderTargetRelation {
    let configured = configured.as_str();
    let reported = reported.as_str();
    if reported == configured {
        return ProviderTargetRelation::Exact;
    }
    if !configured.is_empty()
        && let Some(remainder) = reported.strip_prefix(configured)
        && let Some(qualifier) = remainder.strip_prefix('-')
        && is_dated_snapshot_qualifier(qualifier)
    {
        return ProviderTargetRelation::AliasConcretion;
    }
    ProviderTargetRelation::DifferentLineage
}

/// Whether a trailing segment is a provider dated-snapshot qualifier.
///
/// Two shapes are admitted, matching the two forms providers publish for a
/// pinned snapshot of one model family: `YYYYMMDD` and `YYYY-MM-DD`. Calendar
/// validity is deliberately not checked — the shape alone separates a dated
/// snapshot from a family or delivery-variant extension, and rejecting an
/// implausible date would add a second rule without adding a distinction.
fn is_dated_snapshot_qualifier(qualifier: &str) -> bool {
    let bytes = qualifier.as_bytes();
    match bytes.len() {
        8 => bytes.iter().all(u8::is_ascii_digit),
        10 => bytes
            .iter()
            .enumerate()
            .all(|(position, byte)| match position {
                4 | 7 => *byte == b'-',
                _ => byte.is_ascii_digit(),
            }),
        _ => false,
    }
}

/// Why one exchange ended without a definitive provider response.
///
/// A projection of the runtime's `LossCause` down to a stable token: the
/// runtime's own variants retain provider-controlled transport and parser
/// text, which never reaches operator telemetry (INV-035).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BoundaryLossCode {
    /// Cancellation fired after send commenced.
    CancellationRequested,
    /// A local timeout elapsed with no definitive response.
    TimedOut,
    /// Transport failure that cannot be proven to precede acceptance.
    TransportFailed,
    /// Response headers arrived but the body was lost.
    ResponseBodyLost,
    /// A success-status body was not the provider's completion material.
    ResponseUnintelligible,
    /// The response carried a status outside the provider's contract.
    UnexpectedHttpStatus,
    /// The event stream ended without its terminal marker.
    StreamEndedWithoutTerminalMarker,
    /// The event stream violated its protocol.
    StreamProtocolViolation,
}

impl BoundaryLossCode {
    /// The stable operator-facing token for this loss.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CancellationRequested => "boundary_loss_cancellation_requested",
            Self::TimedOut => "boundary_loss_timed_out",
            Self::TransportFailed => "boundary_loss_transport_failed",
            Self::ResponseBodyLost => "boundary_loss_response_body_lost",
            Self::ResponseUnintelligible => "boundary_loss_response_unintelligible",
            Self::UnexpectedHttpStatus => "boundary_loss_unexpected_http_status",
            Self::StreamEndedWithoutTerminalMarker => "boundary_loss_stream_incomplete",
            Self::StreamProtocolViolation => "boundary_loss_stream_protocol_violation",
        }
    }

    const fn of(cause: &LossCause) -> Self {
        match cause {
            LossCause::CancellationRequested => Self::CancellationRequested,
            LossCause::TimedOut(_) => Self::TimedOut,
            LossCause::TransportFailed(_) => Self::TransportFailed,
            LossCause::ResponseBodyLost(_) => Self::ResponseBodyLost,
            LossCause::ResponseUnintelligible { .. } => Self::ResponseUnintelligible,
            LossCause::UnexpectedHttpStatus => Self::UnexpectedHttpStatus,
            LossCause::StreamEndedWithoutTerminalMarker { .. } => {
                Self::StreamEndedWithoutTerminalMarker
            }
            LossCause::StreamProtocolViolation { .. } => Self::StreamProtocolViolation,
        }
    }
}

/// The stable, sanitized cause of one model-call outcome or bridge defect.
///
/// Every value renders as a fixed operator-facing token
/// ([`as_str`](Self::as_str)); no provider response text, request or response
/// body, credential material, or user content can reach it (INV-035). The
/// runtime's own exhaustive `ProviderErrorKind` classification is carried
/// verbatim rather than restated, so the adapter taxonomy of
/// docs/spec/runtime-substrate.md and this operator vocabulary cannot drift
/// apart.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelCallCauseCode {
    /// The exchange completed with usable assistant material.
    Completed,
    /// The provider reported a refusal outcome.
    Refused,
    /// A definitive provider error response.
    ProviderError(ProviderErrorKind),
    /// A definitive provider cancellation response.
    CancellationConfirmed,
    /// Cancellation fired before any send was attempted.
    CancelledBeforeSend,
    /// The connection failed before any request byte was written.
    ConnectFailed,
    /// An incomplete write the provider contract proves was unacceptable.
    SendIncompleteProvenUnacceptable,
    /// The exchange was lost after possible provider acceptance.
    BoundaryLoss(BoundaryLossCode),
    /// Capability preparation reported that the adapter does not support the
    /// requested operation.
    UnsupportedOperation,
    /// The pinned credential reference could not be resolved during
    /// preparation.
    CredentialUnavailable(CredentialAccessCode),
    /// A resolved credential cannot authenticate the constructed request.
    CredentialUnusable,
    /// The provider served a model from a different lineage than the
    /// configured target.
    ProviderTargetSubstituted,
    /// Completion tool material could not form a bounded domain proposal
    /// batch.
    UnrepresentableToolMaterial,
    /// The completion's finish reason contradicted its own content.
    FinishContradictsContent,
    /// A durably resolved target had no runtime mapping.
    UnconfiguredTarget,
    /// Runtime preparation reported a local adapter defect.
    PreparationDefect,
    /// The runtime returned a different caller-owned correlation identity.
    CorrelationMismatch,
    /// Durable authorization did not match the prepared one-shot request.
    AuthorizationMismatch,
    /// A runtime observation did not carry the caller-owned call identity.
    ObservationCorrelationMismatch,
    /// Definitive response material is outside the supported slice.
    UnsupportedCompletionMaterial,
    /// A runtime text part cannot construct exact domain assistant text.
    InvalidAssistantText,
    /// A checked application schema could not form a runtime JSON value.
    InvalidToolSchema,
    /// Runtime tool material could not form a bounded domain proposal.
    InvalidToolProposal,
}

impl ModelCallCauseCode {
    /// The stable operator-facing token for this cause.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Refused => "provider_refused",
            Self::ProviderError(kind) => provider_error_token(kind),
            Self::CancellationConfirmed => "provider_cancellation_confirmed",
            Self::CancelledBeforeSend => "cancelled_before_send",
            Self::ConnectFailed => "connect_failed",
            Self::SendIncompleteProvenUnacceptable => "send_incomplete_proven_unacceptable",
            Self::BoundaryLoss(code) => code.as_str(),
            Self::UnsupportedOperation => "unsupported_operation",
            Self::CredentialUnavailable(code) => code.as_str(),
            Self::CredentialUnusable => "credential_unusable",
            Self::ProviderTargetSubstituted => "provider_target_substituted",
            Self::UnrepresentableToolMaterial => "unrepresentable_tool_material",
            Self::FinishContradictsContent => "finish_contradicts_content",
            Self::UnconfiguredTarget => "unconfigured_target",
            Self::PreparationDefect => "preparation_defect",
            Self::CorrelationMismatch => "correlation_mismatch",
            Self::AuthorizationMismatch => "authorization_mismatch",
            Self::ObservationCorrelationMismatch => "observation_correlation_mismatch",
            Self::UnsupportedCompletionMaterial => "unsupported_completion_material",
            Self::InvalidAssistantText => "invalid_assistant_text",
            Self::InvalidToolSchema => "invalid_tool_schema",
            Self::InvalidToolProposal => "invalid_tool_proposal",
        }
    }
}

/// Why a pinned credential reference could not be resolved.
///
/// A projection of the runtime's `CredentialAccessFailure` down to a stable
/// token. The reference itself is deliberately not carried: it is non-secret
/// but names deployment configuration, and the failure class is what an
/// operator acts on.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CredentialAccessCode {
    /// No delivery artifact is mapped to the reference.
    Unmapped,
    /// The mapped artifact could not be reached.
    Unavailable,
    /// The artifact was present but could not be read as a value.
    Unreadable,
}

impl CredentialAccessCode {
    /// The stable operator-facing token for this access failure.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unmapped => "credential_unmapped",
            Self::Unavailable => "credential_unavailable",
            Self::Unreadable => "credential_unreadable",
        }
    }

    const fn of(failure: CredentialAccessFailure) -> Self {
        match failure {
            CredentialAccessFailure::Unmapped => Self::Unmapped,
            CredentialAccessFailure::Unavailable => Self::Unavailable,
            CredentialAccessFailure::Unreadable => Self::Unreadable,
        }
    }
}

/// The sanitized cause of one trustworthy pre-send preparation failure.
///
/// The runtime's own closed `PreparationFailure` vocabulary maps here without
/// its rendered detail strings, which are adapter- and provider-controlled.
const fn preparation_failure_cause(failure: &PreparationFailure) -> ModelCallCauseCode {
    match failure {
        PreparationFailure::UnsupportedOperation { .. } => ModelCallCauseCode::UnsupportedOperation,
        PreparationFailure::CredentialUnavailable { error } => {
            ModelCallCauseCode::CredentialUnavailable(CredentialAccessCode::of(error.failure))
        }
        PreparationFailure::CredentialUnusable { .. } => ModelCallCauseCode::CredentialUnusable,
    }
}

/// The stable token for one runtime provider-error classification.
///
/// Kept as an exhaustive `match` so a new `ProviderErrorKind` cannot reach
/// operator telemetry without a deliberate token.
const fn provider_error_token(kind: ProviderErrorKind) -> &'static str {
    match kind {
        ProviderErrorKind::CredentialRejected => "provider_credential_rejected",
        ProviderErrorKind::PermissionDenied => "provider_permission_denied",
        ProviderErrorKind::InvalidRequest => "provider_invalid_request",
        ProviderErrorKind::TargetNotFound => "provider_target_not_found",
        ProviderErrorKind::RequestTooLarge => "provider_request_too_large",
        ProviderErrorKind::RateLimited => "provider_rate_limited",
        ProviderErrorKind::QuotaExhausted => "provider_quota_exhausted",
        ProviderErrorKind::Overloaded => "provider_overloaded",
        ProviderErrorKind::ProviderInternal => "provider_internal",
        ProviderErrorKind::Unrecognized => "provider_unrecognized_error",
    }
}

/// Bounds a provider-reported identity before it reaches operator telemetry.
///
/// The provider controls the reported spelling, so the diagnostic projection
/// is truncated to [`DIAGNOSTIC_MODEL_IDENTITY_LIMIT`] bytes on a character
/// boundary. The value is already credential-redacted by the adapter
/// (docs/spec/runtime-substrate.md); this bound keeps a hostile length from
/// reaching a log line.
fn diagnostic_model_identity(reported: &str) -> String {
    if reported.len() <= DIAGNOSTIC_MODEL_IDENTITY_LIMIT {
        return reported.to_owned();
    }
    let mut boundary = DIAGNOSTIC_MODEL_IDENTITY_LIMIT;
    while boundary > 0 && !reported.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut bounded = String::from(reported.get(..boundary).unwrap_or_default());
    bounded.push_str("… [truncated]");
    bounded
}

#[derive(Clone, Copy)]
struct PreparedBinding {
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    call: ModelCallId,
    selection: FrozenModelSelection,
    target: ResolvedProviderTarget,
    frontier: ContextFrontierId,
}

impl PreparedBinding {
    fn matches(&self, authorized: &AuthorizedModelCall) -> bool {
        self.session == authorized.session()
            && self.turn == authorized.turn()
            && self.attempt == authorized.attempt().id()
            && self.call == authorized.call().id()
            && self.selection == authorized.call().selection()
            && self.target == authorized.call().target()
            && self.frontier == authorized.call().frontier().snapshot()
    }
}

/// Opaque runtime capability plus the application facts it was prepared from.
pub struct RuntimeModelCallCapability<Prepared> {
    prepared: Prepared,
    binding: PreparedBinding,
    resolved_target: ResolvedTarget,
}

/// Sanitized adapter defect; provider response text and credentials are never
/// retained in this error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeModelCallProviderError {
    /// A durably resolved target had no matching runtime mapping.
    UnconfiguredTarget,
    /// Runtime preparation reported a local adapter defect.
    PreparationDefect,
    /// The runtime returned a different caller-owned correlation identity.
    CorrelationMismatch,
    /// Durable authorization did not match the prepared one-shot request.
    AuthorizationMismatch,
    /// A runtime observation did not carry the caller-owned call identity.
    ObservationCorrelationMismatch,
    /// The provider served a model from a different lineage than the
    /// configured target — a substitution the daemon never authorized, and a
    /// distinct outcome from an alias made concrete.
    ProviderTargetSubstituted,
    /// Definitive response material is outside the first text-only slice.
    UnsupportedCompletionMaterial,
    /// A runtime text part cannot construct exact domain assistant text.
    InvalidAssistantText,
    /// A checked application schema could not form a runtime JSON value.
    InvalidToolSchema,
    /// Runtime tool material could not form a bounded domain proposal.
    InvalidToolProposal,
}

impl fmt::Display for RuntimeModelCallProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnconfiguredTarget => "resolved model target has no runtime mapping",
            Self::PreparationDefect => "model runtime preparation reported a defect",
            Self::CorrelationMismatch => "model runtime returned a different correlation",
            Self::AuthorizationMismatch => {
                "authorized model call differs from the prepared capability"
            }
            Self::ObservationCorrelationMismatch => {
                "model runtime observation carried a different correlation"
            }
            Self::ProviderTargetSubstituted => {
                "provider served a different model lineage than the configured target"
            }
            Self::UnsupportedCompletionMaterial => {
                "provider completion contains unsupported assistant material"
            }
            Self::InvalidAssistantText => "provider completion contains invalid assistant text",
            Self::InvalidToolSchema => "application tool schema is invalid at the runtime bridge",
            Self::InvalidToolProposal => "provider completion contains an invalid tool proposal",
        })
    }
}

impl Error for RuntimeModelCallProviderError {}

impl RuntimeModelCallProviderError {
    /// The stable, sanitized operator-facing cause of this fail-closed
    /// outcome.
    ///
    /// The shared operator taxonomy
    /// (docs/spec/runtime-substrate.md#operator-failure-taxonomy) says only
    /// *how bad* a failure is; this says *what happened*, without exposing
    /// provider text or user content.
    pub const fn cause_code(self) -> ModelCallCauseCode {
        match self {
            Self::UnconfiguredTarget => ModelCallCauseCode::UnconfiguredTarget,
            Self::PreparationDefect => ModelCallCauseCode::PreparationDefect,
            Self::CorrelationMismatch => ModelCallCauseCode::CorrelationMismatch,
            Self::AuthorizationMismatch => ModelCallCauseCode::AuthorizationMismatch,
            Self::ObservationCorrelationMismatch => {
                ModelCallCauseCode::ObservationCorrelationMismatch
            }
            Self::ProviderTargetSubstituted => ModelCallCauseCode::ProviderTargetSubstituted,
            Self::UnsupportedCompletionMaterial => {
                ModelCallCauseCode::UnsupportedCompletionMaterial
            }
            Self::InvalidAssistantText => ModelCallCauseCode::InvalidAssistantText,
            Self::InvalidToolSchema => ModelCallCauseCode::InvalidToolSchema,
            Self::InvalidToolProposal => ModelCallCauseCode::InvalidToolProposal,
        }
    }
}

impl ClassifyOperatorFailure for RuntimeModelCallProviderError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

/// Application-port adapter over one provider-neutral model runtime.
pub struct RuntimeModelCallProvider<R> {
    runtime: Arc<R>,
    models: RuntimeModelCatalog,
    text_deltas: Arc<dyn ProviderTextDeltaSink>,
}

struct AcceptanceObservations<AcceptancePossible, Correlation> {
    expected_correlation: Correlation,
    correlation_mismatch: bool,
    acceptance_possible: Option<AcceptancePossible>,
    text_deltas: Option<ProviderTextDeltaContext>,
    observations: Vec<Observation<Correlation>>,
}

struct ProviderTextDeltaContext {
    session: SessionId,
    turn: TurnId,
    call: ModelCallId,
    sink: Arc<dyn ProviderTextDeltaSink>,
}

impl<AcceptancePossible, Correlation> ObservationSink<Correlation>
    for AcceptanceObservations<AcceptancePossible, Correlation>
where
    AcceptancePossible: FnOnce(),
    Correlation: PartialEq,
{
    fn observe(&mut self, observation: Observation<Correlation>) {
        if observation.correlation != self.expected_correlation {
            self.correlation_mismatch = true;
            self.observations.push(observation);
            return;
        }
        if matches!(&observation.fact, ObservationFact::SendCommenced)
            && let Some(acceptance_possible) = self.acceptance_possible.take()
        {
            acceptance_possible();
        }
        if let (Some(context), ObservationFact::TextDelta { index, text }) =
            (&self.text_deltas, &observation.fact)
        {
            context.sink.publish(ProviderTextDelta::new(
                context.session,
                context.turn,
                context.call,
                *index,
                text.clone(),
            ));
        }
        self.observations.push(observation);
    }
}

impl<R> RuntimeModelCallProvider<R> {
    /// Supplies the runtime and immutable target mapping.
    pub fn new(runtime: R, models: RuntimeModelCatalog) -> Self {
        Self {
            runtime: Arc::new(runtime),
            models,
            text_deltas: Arc::new(DiscardProviderTextDeltas),
        }
    }

    /// Delivers already-redacted provider text observations to an ephemeral
    /// presentation sink while preserving the evidence path unchanged.
    pub fn with_text_delta_sink(
        mut self,
        text_deltas: impl ProviderTextDeltaSink + 'static,
    ) -> Self {
        self.text_deltas = Arc::new(text_deltas);
        self
    }
}

impl<R> Clone for RuntimeModelCallProvider<R> {
    fn clone(&self) -> Self {
        Self {
            runtime: Arc::clone(&self.runtime),
            models: self.models.clone(),
            text_deltas: Arc::clone(&self.text_deltas),
        }
    }
}

impl<R> fmt::Debug for RuntimeModelCallProvider<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeModelCallProvider")
            .field("runtime", &"[provider runtime]")
            .field("models", &self.models)
            .finish()
    }
}

/// Sanitized exact-count adapter failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeInputTokenCountError {
    /// The durable target has no runtime mapping.
    UnconfiguredTarget,
    /// A checked application schema could not form runtime JSON.
    InvalidToolSchema,
    /// The runtime returned a different caller-owned correlation.
    CorrelationMismatch,
    /// The provider-native count request did not return a validated count.
    CountFailed,
}

impl fmt::Display for RuntimeInputTokenCountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("exact model input token counting failed")
    }
}

impl Error for RuntimeInputTokenCountError {}

impl ClassifyOperatorFailure for RuntimeInputTokenCountError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::CountFailed => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::UnconfiguredTarget | Self::InvalidToolSchema | Self::CorrelationMismatch => {
                OperatorFailureClass::CallerOrHubBug
            }
        }
    }
}

impl<R> ModelCallInputTokenCounter for RuntimeModelCallProvider<R>
where
    R: signalbox_model_runtime::ModelInputTokenCounter<ModelCallId> + Send + Sync,
{
    type Error = RuntimeInputTokenCountError;

    async fn count_input_tokens<Cancellation>(
        &self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> Result<ModelCallInputTokenCount, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        let request = operation.request();
        let call = request.call();
        let correlation = call.id();
        let definition = self
            .models
            .resolve(call.target())
            .ok_or(RuntimeInputTokenCountError::UnconfiguredTarget)?;
        let messages = render_runtime_messages(operation.messages());
        let tools = operation
            .tools()
            .iter()
            .map(|definition| {
                let schema = decode_checked_raw_json(definition.input_schema().as_str())
                    .map_err(|_| RuntimeInputTokenCountError::InvalidToolSchema)?;
                Ok(ToolDefinition::with_raw_schema(
                    definition.name().as_str(),
                    definition.description(),
                    schema,
                ))
            })
            .collect::<Result<Vec<_>, RuntimeInputTokenCountError>>()?;
        let mut runtime_operation = ModelOperation::new(
            correlation,
            CredentialReference::new(operation.credential_reference().as_str().to_owned()),
            RequestedTarget::new(render_requested_target(call.selection())),
            ResolvedTarget::new(definition.provider_model().to_owned()),
            messages,
            ModelSettings::new(definition.max_output_tokens()),
        );
        runtime_operation.system = operation.system_prompt().map(str::to_owned);
        runtime_operation.tools = tools;
        runtime_operation.delivery = DeliveryMode::Streamed;
        match self
            .runtime
            .count_input_tokens(runtime_operation, CancellationSignal::when(cancellation))
            .await
        {
            signalbox_model_runtime::InputTokenCountOutcome::Counted {
                correlation: returned,
                input_tokens,
            } if returned == correlation => Ok(ModelCallInputTokenCount::Counted(input_tokens)),
            signalbox_model_runtime::InputTokenCountOutcome::Cancelled {
                correlation: returned,
            } if returned == correlation => Ok(ModelCallInputTokenCount::Cancelled),
            signalbox_model_runtime::InputTokenCountOutcome::Failed {
                correlation: returned,
            } if returned == correlation => Err(RuntimeInputTokenCountError::CountFailed),
            signalbox_model_runtime::InputTokenCountOutcome::Counted { .. }
            | signalbox_model_runtime::InputTokenCountOutcome::Cancelled { .. }
            | signalbox_model_runtime::InputTokenCountOutcome::Failed { .. } => {
                Err(RuntimeInputTokenCountError::CorrelationMismatch)
            }
        }
    }
}

impl<R> ModelCallProvider for RuntimeModelCallProvider<R>
where
    R: ModelRuntime<ModelCallId> + Send + Sync,
{
    type Capability = RuntimeModelCallCapability<R::Prepared>;
    type Error = RuntimeModelCallProviderError;

    async fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        let request = operation.request();
        let call = request.call();
        let credential =
            CredentialReference::new(operation.credential_reference().as_str().to_owned());
        let correlation = call.id();
        let definition = self.models.resolve(call.target()).ok_or_else(|| {
            fail_closed(
                correlation,
                RuntimeModelCallProviderError::UnconfiguredTarget,
                None,
            )
        })?;
        let binding = PreparedBinding {
            session: request.session(),
            turn: request.turn(),
            attempt: request.attempt(),
            call: correlation,
            selection: call.selection(),
            target: call.target(),
            frontier: call.frontier().snapshot(),
        };
        let messages = render_runtime_messages(operation.messages());
        let tools = operation
            .tools()
            .iter()
            .map(|definition| {
                let schema =
                    decode_checked_raw_json(definition.input_schema().as_str()).map_err(|_| {
                        fail_closed(
                            correlation,
                            RuntimeModelCallProviderError::InvalidToolSchema,
                            None,
                        )
                    })?;
                Ok(ToolDefinition::with_raw_schema(
                    definition.name().as_str(),
                    definition.description(),
                    schema,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let resolved_target = ResolvedTarget::new(definition.provider_model().to_owned());
        let mut runtime_operation = ModelOperation::new(
            correlation,
            credential,
            RequestedTarget::new(render_requested_target(call.selection())),
            resolved_target.clone(),
            messages,
            ModelSettings::new(definition.max_output_tokens()),
        );
        // The session system prompt frozen through the calling turn's
        // defaults epoch rides every operation; adapters translate a `None`
        // as no system instructions (docs/spec/sessions-and-transcript.md).
        runtime_operation.system = operation.system_prompt().map(str::to_owned);
        runtime_operation.tools = tools;
        runtime_operation.delivery = DeliveryMode::Streamed;
        match self
            .runtime
            .prepare(runtime_operation, CancellationSignal::when(cancellation))
            .await
        {
            PreparationOutcome::Prepared(prepared) => Ok(ModelCallCapabilityPreparation::Ready(
                RuntimeModelCallCapability {
                    prepared,
                    binding,
                    resolved_target,
                },
            )),
            PreparationOutcome::Cancelled {
                correlation: returned,
            } => {
                require_correlation(correlation, returned)?;
                Ok(ModelCallCapabilityPreparation::Cancelled)
            }
            PreparationOutcome::Failed {
                correlation: returned,
                failure,
            } => {
                require_correlation(correlation, returned)?;
                tracing::warn!(
                    cause_code = preparation_failure_cause(&failure).as_str(),
                    model_call_id = %correlation.as_uuid(),
                    "model runtime reported a trustworthy capability-preparation failure"
                );
                Ok(ModelCallCapabilityPreparation::KnownFailure)
            }
            PreparationOutcome::Defect {
                correlation: returned,
                ..
            } => {
                require_correlation(correlation, returned)?;
                Err(fail_closed(
                    correlation,
                    RuntimeModelCallProviderError::PreparationDefect,
                    None,
                ))
            }
        }
    }

    async fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> Result<signalbox_domain::CorrelatedModelCallTerminalObservation, Self::Error>
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        let correlation = authorized.call().id();
        if !capability.binding.matches(&authorized) {
            return Err(fail_closed(
                correlation,
                RuntimeModelCallProviderError::AuthorizationMismatch,
                None,
            ));
        }
        let mut observations = AcceptanceObservations {
            expected_correlation: correlation,
            correlation_mismatch: false,
            acceptance_possible: Some(acceptance_possible),
            text_deltas: Some(ProviderTextDeltaContext {
                session: capability.binding.session,
                turn: capability.binding.turn,
                call: capability.binding.call,
                sink: Arc::clone(&self.text_deltas),
            }),
            observations: Vec::new(),
        };
        let report = self
            .runtime
            .execute(
                capability.prepared,
                &mut observations,
                CancellationSignal::when(cancellation),
            )
            .await;
        require_correlation(correlation, report.correlation)?;
        if observations.correlation_mismatch {
            return Err(fail_closed(
                correlation,
                RuntimeModelCallProviderError::ObservationCorrelationMismatch,
                None,
            ));
        }
        let classified = classify_terminal(
            report.evidence,
            &observations.observations,
            &capability.resolved_target,
        )
        .map_err(|failure| {
            fail_closed(correlation, failure.error, failure.served_target.as_deref())
        })?;
        report_classified_outcome(correlation, &classified);
        Ok(authorized
            .observation_correlation()
            .bind_terminal_observation(classified.observation))
    }
}

/// Records one fail-closed bridge outcome for operators and returns it.
///
/// Sanitized by construction: the correlation identity, the stable cause
/// token, and — for a substitution — the bounded provider identity that
/// actually served are the only fields, so no provider text, response body,
/// credential material, or user content can reach telemetry (INV-035).
fn fail_closed(
    correlation: ModelCallId,
    error: RuntimeModelCallProviderError,
    served_target: Option<&str>,
) -> RuntimeModelCallProviderError {
    match served_target {
        Some(served_target) => tracing::error!(
            failure_class = ?error.operator_failure_class(),
            cause_code = error.cause_code().as_str(),
            model_call_id = %correlation.as_uuid(),
            served_provider_target = served_target,
            "model call failed closed at the runtime bridge"
        ),
        None => tracing::error!(
            failure_class = ?error.operator_failure_class(),
            cause_code = error.cause_code().as_str(),
            model_call_id = %correlation.as_uuid(),
            "model call failed closed at the runtime bridge"
        ),
    }
    error
}

/// Records one classified terminal outcome for operators.
fn report_classified_outcome(correlation: ModelCallId, classified: &TerminalClassification) {
    if let Some(concrete_target) = &classified.concrete_target {
        tracing::info!(
            model_call_id = %correlation.as_uuid(),
            concrete_provider_target = concrete_target.as_str(),
            "provider served the configured target in its concrete dated form"
        );
    }
    match classified.observation {
        ModelCallTerminalObservation::Completed { .. }
        | ModelCallTerminalObservation::CompletedWithTools { .. } => {
            tracing::debug!(
                cause_code = classified.cause.as_str(),
                model_call_id = %correlation.as_uuid(),
                "model call completed"
            );
        }
        _ => {
            tracing::warn!(
                cause_code = classified.cause.as_str(),
                model_call_id = %correlation.as_uuid(),
                "model call produced no assistant material"
            );
        }
    }
}

fn render_runtime_messages(messages: &[ModelConversationMessage]) -> Vec<ConversationMessage> {
    let mut rendered = Vec::new();
    let mut assistant_call = None;
    let mut collecting_tool_results = false;
    for message in messages {
        match message {
            ModelConversationMessage::ModelIdentityChanged {
                defaults_version,
                selected,
                ..
            } => {
                rendered.push(ConversationMessage::user_text(format!(
                    "{MODEL_IDENTITY_CHANGE_MESSAGE} {} (session defaults epoch {}).",
                    selected.into_uuid(),
                    defaults_version.as_u64()
                )));
                assistant_call = None;
                collecting_tool_results = false;
            }
            ModelConversationMessage::ContextSummary { content, .. } => {
                rendered.push(ConversationMessage::user_text(format!(
                    "{CONTEXT_SUMMARY_MESSAGE}\n{}",
                    content.as_str()
                )));
                assistant_call = None;
                collecting_tool_results = false;
            }
            ModelConversationMessage::User { content, .. } => {
                rendered.push(ConversationMessage::user_text(content.text().as_str()));
                assistant_call = None;
                collecting_tool_results = false;
            }
            ModelConversationMessage::Assistant {
                producing_call,
                content,
                ..
            } => {
                if assistant_call == Some(*producing_call) {
                    if let Some(message) = rendered.last_mut() {
                        message
                            .parts
                            .push(MessagePart::Text(content.as_str().to_owned()));
                    } else {
                        rendered.push(ConversationMessage::assistant_text(content.as_str()));
                    }
                } else {
                    rendered.push(ConversationMessage::assistant_text(content.as_str()));
                    assistant_call = Some(*producing_call);
                }
                collecting_tool_results = false;
            }
            ModelConversationMessage::AssistantToolUse {
                producing_call,
                request,
                ..
            } => {
                let part = MessagePart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new(request.id().into_uuid().to_string()),
                    name: RuntimeToolName::new(request.name().as_str()),
                    arguments_json: replay_safe_arguments(request),
                });
                if assistant_call == Some(*producing_call) {
                    if let Some(message) = rendered.last_mut() {
                        message.parts.push(part);
                    } else {
                        rendered.push(ConversationMessage {
                            role: ConversationRole::Assistant,
                            parts: vec![part],
                        });
                    }
                } else {
                    rendered.push(ConversationMessage {
                        role: ConversationRole::Assistant,
                        parts: vec![part],
                    });
                    assistant_call = Some(*producing_call);
                }
                collecting_tool_results = false;
            }
            ModelConversationMessage::ToolResult {
                request, content, ..
            } => {
                let (content, is_error) = render_tool_result(content);
                let part = MessagePart::ToolResult(ToolResultRecord {
                    tool_call_id: ToolCallId::new(request.into_uuid().to_string()),
                    content,
                    is_error,
                });
                if collecting_tool_results {
                    if let Some(message) = rendered.last_mut() {
                        message.parts.push(part);
                    } else {
                        rendered.push(ConversationMessage {
                            role: ConversationRole::User,
                            parts: vec![part],
                        });
                    }
                } else {
                    rendered.push(ConversationMessage {
                        role: ConversationRole::User,
                        parts: vec![part],
                    });
                }
                assistant_call = None;
                collecting_tool_results = true;
            }
            ModelConversationMessage::ImportedUser { content, .. } => {
                rendered.push(ConversationMessage::user_text(content.as_str()));
                assistant_call = None;
                collecting_tool_results = false;
            }
            ModelConversationMessage::ImportedAssistant { content, .. } => {
                rendered.push(ConversationMessage::assistant_text(content.as_str()));
                assistant_call = None;
                collecting_tool_results = false;
            }
        }
    }
    rendered
}

fn replay_safe_arguments(request: &signalbox_domain::ToolRequest) -> String {
    if request.arguments().kind() == ToolArgumentsKind::Json
        && decode_checked_raw_json(request.arguments().as_str()).is_ok_and(|value| {
            value.get().bytes().find(|byte| !byte.is_ascii_whitespace()) == Some(b'{')
        })
    {
        request.arguments().as_str().to_owned()
    } else {
        // Exact bytes remain durable authority. Replayed function arguments
        // must be an object even when the provider originally supplied a
        // scalar, array, or undecodable value.
        String::from(r#"{"signalbox_invalid_arguments":true}"#)
    }
}

fn decode_checked_raw_json(
    value: &str,
) -> Result<Box<serde_json::value::RawValue>, serde_json::Error> {
    serde_json::value::RawValue::from_string(value.to_owned())
}

/// A correlation mismatch is a fail-closed bridge defect like any other, so
/// it is recorded through [`fail_closed`] rather than returned silently.
fn require_correlation(
    expected: ModelCallId,
    returned: ModelCallId,
) -> Result<(), RuntimeModelCallProviderError> {
    if expected == returned {
        Ok(())
    } else {
        Err(fail_closed(
            expected,
            RuntimeModelCallProviderError::CorrelationMismatch,
            None,
        ))
    }
}

fn render_requested_target(selection: FrozenModelSelection) -> String {
    match selection {
        FrozenModelSelection::Direct(direct) => format!("direct:{}", direct.into_uuid()),
        FrozenModelSelection::FrozenAlias { alias, definition } => format!(
            "alias:{}@direct:{}",
            alias.into_uuid(),
            definition.selected().into_uuid()
        ),
    }
}

fn render_tool_result(content: &ModelToolResultContent) -> (String, bool) {
    match content {
        ModelToolResultContent::Success(ToolResultContent::Text(text)) => {
            (text.as_str().to_owned(), false)
        }
        ModelToolResultContent::ExecutionError(error) => {
            let kind = match error.kind() {
                ToolExecutionErrorKind::UnknownTool => "unknown_tool",
                ToolExecutionErrorKind::InvalidArguments => "invalid_arguments",
                ToolExecutionErrorKind::ExecutionFailed => "execution_failed",
                ToolExecutionErrorKind::ResultTooLarge => "result_too_large",
                ToolExecutionErrorKind::CrashLost => "crash_lost",
            };
            (
                serde_json::json!({
                    "error": {
                        "kind": kind,
                        "detail": error.detail().map(signalbox_domain::ToolExecutionErrorDetail::as_str),
                    }
                })
                .to_string(),
                true,
            )
        }
        ModelToolResultContent::Denied { reason } => (
            serde_json::json!({
                "error": {
                    "kind": "denied",
                    "detail": reason.as_ref().map(signalbox_domain::ToolDenialReason::as_str),
                }
            })
            .to_string(),
            true,
        ),
        ModelToolResultContent::ClosedByTurnEnd => (
            serde_json::json!({
                "error": {
                    "kind": "closed_by_turn_end",
                    "detail": null,
                }
            })
            .to_string(),
            true,
        ),
    }
}

/// One classified terminal outcome plus the sanitized diagnostics that
/// explain it to an operator.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TerminalClassification {
    observation: ModelCallTerminalObservation,
    cause: ModelCallCauseCode,
    /// The concrete provider identity that served the exchange, retained
    /// only when it was the configured target made concrete rather than the
    /// exact configured spelling.
    concrete_target: Option<String>,
}

/// One fail-closed classification outcome plus the sanitized diagnostics that
/// explain it.
///
/// A substitution carries the bounded identity that actually served, because
/// the recorded decision makes substitution a separate *recorded* outcome:
/// the token alone would leave an operator unable to name the model the
/// provider used.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ClassificationFailure {
    error: RuntimeModelCallProviderError,
    served_target: Option<String>,
}

impl ClassificationFailure {
    /// A defect with no served identity to record.
    const fn bare(error: RuntimeModelCallProviderError) -> Self {
        Self {
            error,
            served_target: None,
        }
    }
}

fn classify_terminal(
    evidence: TerminalEvidence,
    observations: &[Observation<ModelCallId>],
    configured_target: &ResolvedTarget,
) -> Result<TerminalClassification, ClassificationFailure> {
    // docs/spec/model-call-execution.md: an alias resolved to its own
    // canonical dated form is the same logical target and is accepted with
    // the concrete identity recorded as evidence; a served identity from
    // another lineage is a substitution the daemon never authorized and is a
    // separate outcome, never collapsed into an ordinary provider failure.
    let mut concrete_target = None;
    for reported in reported_identities(&evidence, observations) {
        match relate_provider_target(configured_target, reported) {
            ProviderTargetRelation::Exact => {}
            ProviderTargetRelation::AliasConcretion => {
                concrete_target = Some(diagnostic_model_identity(reported.as_str()));
            }
            ProviderTargetRelation::DifferentLineage => {
                return Err(ClassificationFailure {
                    error: RuntimeModelCallProviderError::ProviderTargetSubstituted,
                    served_target: Some(diagnostic_model_identity(reported.as_str())),
                });
            }
        }
    }

    let classify = |observation, cause| {
        Ok(TerminalClassification {
            observation,
            cause,
            concrete_target: concrete_target.clone(),
        })
    };

    match evidence {
        TerminalEvidence::Completed(completion) => {
            let finish = completion.finish;
            let mut response_parts = Vec::new();
            let mut text_parts = Vec::new();
            let mut tool_count = 0usize;
            for part in completion.content {
                match part {
                    AssistantPart::Text(text) if text.is_empty() => {}
                    AssistantPart::Text(text) => {
                        let text = AssistantText::try_new(text).map_err(|_| {
                            ClassificationFailure::bare(
                                RuntimeModelCallProviderError::InvalidAssistantText,
                            )
                        })?;
                        text_parts.push(text.clone());
                        response_parts.push(AssistantResponsePart::Text(text));
                    }
                    AssistantPart::ToolCall(proposal) => {
                        tool_count += 1;
                        let Ok(name) = DomainToolName::try_new(proposal.name.as_str().to_owned())
                        else {
                            return classify(
                                ModelCallTerminalObservation::KnownFailed,
                                ModelCallCauseCode::UnrepresentableToolMaterial,
                            );
                        };
                        let Ok(arguments) = NormalizedToolArguments::try_from_provider_text(
                            proposal.arguments_json,
                        ) else {
                            return classify(
                                ModelCallTerminalObservation::KnownFailed,
                                ModelCallCauseCode::UnrepresentableToolMaterial,
                            );
                        };
                        response_parts.push(AssistantResponsePart::ToolCall(
                            DomainToolCallProposal::new(name, arguments),
                        ));
                    }
                    // Claude 5-family models run adaptive thinking by
                    // default and, with the default omitted display, return
                    // thinking blocks whose text is empty: the block carries
                    // only the provider's replay signature. Signalbox has no
                    // durable thinking representation to replay from, so the
                    // signature is unusable either way and an empty part
                    // carries no transcript content — it is dropped exactly
                    // like an empty text block. The provider documents the
                    // resulting tool continuation as graceful degradation:
                    // a tool-use turn replayed without its thinking block
                    // silently disables thinking for that request instead of
                    // erroring. Thinking with actual text and redacted
                    // thinking still fail closed: discarding them would
                    // silently erase response material.
                    AssistantPart::Thinking { text, .. } if text.is_empty() => {}
                    AssistantPart::Thinking { .. } | AssistantPart::RedactedThinking { .. } => {
                        return Err(ClassificationFailure::bare(
                            RuntimeModelCallProviderError::UnsupportedCompletionMaterial,
                        ));
                    }
                }
            }
            if tool_count == 0 {
                if matches!(finish, CompletionFinish::ToolUse) {
                    return classify(
                        ModelCallTerminalObservation::KnownFailed,
                        ModelCallCauseCode::FinishContradictsContent,
                    );
                }
                classify(
                    ModelCallTerminalObservation::Completed {
                        assistant_text: text_parts,
                    },
                    ModelCallCauseCode::Completed,
                )
            } else {
                if !matches!(finish, CompletionFinish::ToolUse) {
                    return classify(
                        ModelCallTerminalObservation::KnownFailed,
                        ModelCallCauseCode::FinishContradictsContent,
                    );
                }
                let Ok(response) = ToolUsingAssistantResponse::try_from_parts(response_parts)
                else {
                    return classify(
                        ModelCallTerminalObservation::KnownFailed,
                        ModelCallCauseCode::UnrepresentableToolMaterial,
                    );
                };
                classify(
                    ModelCallTerminalObservation::CompletedWithTools { response },
                    ModelCallCauseCode::Completed,
                )
            }
        }
        TerminalEvidence::Refused(_) => classify(
            ModelCallTerminalObservation::Refused,
            ModelCallCauseCode::Refused,
        ),
        TerminalEvidence::ProviderError(error) => classify(
            ModelCallTerminalObservation::KnownFailed,
            ModelCallCauseCode::ProviderError(error.kind),
        ),
        TerminalEvidence::ProvenUnsent(signalbox_model_runtime::ProvenUnsentEvidence { cause }) => {
            match cause {
                UnsentCause::ConnectFailed(_) => classify(
                    ModelCallTerminalObservation::KnownFailed,
                    ModelCallCauseCode::ConnectFailed,
                ),
                UnsentCause::SendIncompleteProvenUnacceptable(_) => classify(
                    ModelCallTerminalObservation::KnownFailed,
                    ModelCallCauseCode::SendIncompleteProvenUnacceptable,
                ),
                UnsentCause::CancelledBeforeSend => classify(
                    ModelCallTerminalObservation::Cancelled,
                    ModelCallCauseCode::CancelledBeforeSend,
                ),
            }
        }
        TerminalEvidence::CancellationConfirmed(_) => classify(
            ModelCallTerminalObservation::Cancelled,
            ModelCallCauseCode::CancellationConfirmed,
        ),
        TerminalEvidence::BoundaryLoss(loss) => classify(
            ModelCallTerminalObservation::Ambiguous,
            ModelCallCauseCode::BoundaryLoss(BoundaryLossCode::of(&loss.cause)),
        ),
    }
}

/// Every provider-reported identity this exchange produced, early
/// observations first and terminal evidence last.
///
/// The mismatch rule of docs/spec/model-call-execution.md is
/// timing-sensitive, so an identity reported before the terminal report is
/// considered on equal footing with the one carried by the report itself.
fn reported_identities<'evidence>(
    evidence: &'evidence TerminalEvidence,
    observations: &'evidence [Observation<ModelCallId>],
) -> impl Iterator<Item = &'evidence ProviderReportedModel> {
    observations
        .iter()
        .filter_map(|observation| match &observation.fact {
            ObservationFact::ProviderModelReported(reported) => Some(reported),
            _ => None,
        })
        .chain(reported_model(evidence))
}

fn reported_model(evidence: &TerminalEvidence) -> Option<&ProviderReportedModel> {
    match evidence {
        TerminalEvidence::Completed(value) => value.reported_model.as_ref(),
        TerminalEvidence::Refused(value) => value.reported_model.as_ref(),
        TerminalEvidence::ProviderError(value) => value.reported_model.as_ref(),
        TerminalEvidence::CancellationConfirmed(value) => value.reported_model.as_ref(),
        TerminalEvidence::BoundaryLoss(value) => value.reported_model.as_ref(),
        TerminalEvidence::ProvenUnsent(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use expect_test::expect;
    use signalbox_application::ModelConversationMessage;
    use signalbox_domain::{
        AssistantText, DirectModelSelection, ImportedText, ImportedTranscriptEntryId, ModelCallId,
        ModelCallTerminalObservation, NormalizedToolArguments, ProviderModelIdentity,
        SemanticTranscriptEntryId, SemanticTranscriptEntryRef, SessionConfigurationDefaultsVersion,
        SessionId, ToolExecutionError, ToolExecutionErrorKind, ToolRequest, ToolRequestId,
        ToolRequestOrdinal, ToolRequestReconstitutionInput, TurnId,
    };
    use signalbox_expect_table::table;
    use signalbox_model_runtime::{
        AssistantPart, BoundaryLossEvidence, CancellationConfirmedEvidence, CompletionEvidence,
        CompletionFinish, ConversationMessage, CredentialAccessError, CredentialAccessFailure,
        ExchangeFacts, LossCause, NativeErrorFacts, Observation, ObservationFact, ObservationSink,
        PreparationFailure, ProvenUnsentEvidence, ProviderErrorEvidence, ProviderErrorKind,
        ProviderReportedModel, RefusalEvidence, TerminalEvidence, TokenUsage, ToolCallId,
        ToolCallProposal, ToolName, TransportFacts, UnsentCause,
    };
    use uuid::Uuid;

    use super::{
        AcceptanceObservations, ProviderTextDelta, ProviderTextDeltaContext, ProviderTextDeltaSink,
        RuntimeModelCallProviderError, RuntimeModelCatalog, RuntimeModelCatalogError,
        RuntimeModelDefinition, classify_terminal, decode_checked_raw_json,
        render_runtime_messages,
    };
    use signalbox_domain::ResolvedProviderTarget;

    fn call() -> ModelCallId {
        ModelCallId::from_uuid(Uuid::from_u128(1))
    }

    /// The exact provider-model spelling one deployment configures.
    fn configured(spelling: &str) -> signalbox_model_runtime::ResolvedTarget {
        signalbox_model_runtime::ResolvedTarget::new(spelling.to_owned())
    }

    /// One provider-reported identity, exactly as observed.
    fn reported(spelling: &str) -> ProviderReportedModel {
        ProviderReportedModel::new(spelling.to_owned())
    }

    fn target(value: u128) -> ResolvedProviderTarget {
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(value)))
    }

    fn source(value: u128) -> SemanticTranscriptEntryRef {
        SemanticTranscriptEntryRef::from_source(
            SessionId::from_uuid(Uuid::from_u128(10)),
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value)),
        )
    }

    fn request(value: u128, arguments: &str) -> ToolRequest {
        ToolRequestReconstitutionInput::new(
            ToolRequestId::from_uuid(Uuid::from_u128(value)),
            SessionId::from_uuid(Uuid::from_u128(10)),
            TurnId::from_uuid(Uuid::from_u128(11)),
            call(),
            ToolRequestOrdinal::from_u32(u32::try_from(value - 20).expect("fixture ordinal")),
            signalbox_domain::ToolName::try_new(String::from("current_time"))
                .expect("fixture name"),
            NormalizedToolArguments::try_from_provider_text(arguments.to_owned())
                .expect("fixture arguments"),
        )
        .into_request()
    }

    /// S28 / INV-038 / INV-039: the outward runtime bridge consumes imported
    /// messages under their rendered role and exact text without consulting or
    /// manufacturing native execution provenance.
    #[test]
    fn s28_inv038_inv039_imported_messages_map_to_provider_neutral_text_roles() {
        let source = SemanticTranscriptEntryRef::from_source(
            SessionId::from_uuid(Uuid::from_u128(2)),
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(3)),
        );
        let imported_entry = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(4));
        let user_text = ImportedText::new(String::from(" \tuser\0text\r\n"));
        let assistant_text = ImportedText::new(String::new());

        assert_eq!(
            render_runtime_messages(&[
                ModelConversationMessage::ImportedUser {
                    source,
                    imported_entry,
                    content: user_text.clone(),
                },
                ModelConversationMessage::ImportedAssistant {
                    source,
                    imported_entry,
                    content: assistant_text.clone(),
                },
            ]),
            vec![
                ConversationMessage::user_text(user_text.as_str()),
                ConversationMessage::assistant_text(assistant_text.as_str()),
            ]
        );
    }

    /// S33 / INV-046: the provider bridge renders the durable identity boundary
    /// as the exact injected user-role session event selected by the recorded
    /// context-lifecycle decision.
    #[test]
    fn s33_inv046_model_identity_boundary_is_an_injected_user_message() {
        let source = source(12);
        let defaults_version = SessionConfigurationDefaultsVersion::try_from_u64(3)
            .expect("the fixture epoch is positive");
        let selected = DirectModelSelection::from_uuid(Uuid::from_u128(13));
        let expected = format!(
            "Signalbox session event: your model identity is now {} (session defaults epoch {}).",
            selected.into_uuid(),
            defaults_version.as_u64()
        );

        assert_eq!(
            render_runtime_messages(&[ModelConversationMessage::ModelIdentityChanged {
                source,
                defaults_version,
                selected,
            }]),
            vec![ConversationMessage::user_text(expected)]
        );
    }

    fn completion(model: &str, content: Vec<AssistantPart>) -> TerminalEvidence {
        completion_with_finish(model, CompletionFinish::EndTurn, content)
    }

    fn completion_with_finish(
        model: &str,
        finish: CompletionFinish,
        content: Vec<AssistantPart>,
    ) -> TerminalEvidence {
        TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(model)),
            finish,
            content,
            usage: TokenUsage::unreported(),
        })
    }

    #[track_caller]
    fn assert_invalid_tool_proposal_closes(evidence: TerminalEvidence) {
        assert_eq!(
            classify_terminal(evidence, &[], &configured("model-exact"))
                .expect("invalid proposal has a durable terminal classification")
                .observation,
            ModelCallTerminalObservation::KnownFailed
        );
    }

    fn tool_completion(model: &str) -> TerminalEvidence {
        TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(model)),
            finish: CompletionFinish::ToolUse,
            content: vec![
                AssistantPart::Text(String::from("checking")),
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("provider-call-opaque"),
                    name: ToolName::new("current_time"),
                    arguments_json: String::from(r#"{ "timezone": "UTC" }"#),
                }),
            ],
            usage: TokenUsage::unreported(),
        })
    }
    /// S10 / INV-002 / INV-005: one provider response and its ordered result
    /// batch remain grouped, while malformed arguments use replay-safe JSON
    /// without replacing their exact durable request evidence.
    #[test]
    fn s10_inv002_inv005_tool_history_is_grouped_and_replay_safe() {
        let first = request(20, "{}");
        let malformed = request(21, "{\"timezone\":");
        let scalar = request(22, "7");
        let deep_arguments = format!("{}0{}", r#"{"nested":"#.repeat(512), "}".repeat(512));
        let deep = request(23, &deep_arguments);
        let messages = [
            signalbox_application::ModelConversationMessage::Assistant {
                source: source(30),
                producing_call: call(),
                content: AssistantText::try_new(String::from("before")).expect("fixture text"),
            },
            signalbox_application::ModelConversationMessage::AssistantToolUse {
                source: source(31),
                producing_call: call(),
                request: first.clone(),
            },
            signalbox_application::ModelConversationMessage::AssistantToolUse {
                source: source(32),
                producing_call: call(),
                request: malformed.clone(),
            },
            signalbox_application::ModelConversationMessage::Assistant {
                source: source(33),
                producing_call: call(),
                content: AssistantText::try_new(String::from("after")).expect("fixture text"),
            },
            signalbox_application::ModelConversationMessage::AssistantToolUse {
                source: source(34),
                producing_call: call(),
                request: scalar.clone(),
            },
            signalbox_application::ModelConversationMessage::AssistantToolUse {
                source: source(38),
                producing_call: call(),
                request: deep.clone(),
            },
            signalbox_application::ModelConversationMessage::Assistant {
                source: source(35),
                producing_call: call(),
                content: AssistantText::try_new(String::from("after")).expect("fixture text"),
            },
            signalbox_application::ModelConversationMessage::ToolResult {
                source: source(36),
                request: first.id(),
                content: signalbox_application::ModelToolResultContent::ExecutionError(
                    ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
                ),
            },
            signalbox_application::ModelConversationMessage::ToolResult {
                source: source(37),
                request: malformed.id(),
                content: signalbox_application::ModelToolResultContent::ExecutionError(
                    ToolExecutionError::new(ToolExecutionErrorKind::InvalidArguments, None),
                ),
            },
            signalbox_application::ModelConversationMessage::ToolResult {
                source: source(38),
                request: scalar.id(),
                content: signalbox_application::ModelToolResultContent::ExecutionError(
                    ToolExecutionError::new(ToolExecutionErrorKind::InvalidArguments, None),
                ),
            },
        ];

        let rendered = render_runtime_messages(&messages);
        assert_eq!(rendered.len(), 2);
        assert_eq!(
            rendered[0].role,
            signalbox_model_runtime::ConversationRole::Assistant
        );
        assert_eq!(rendered[0].parts.len(), 7);
        let signalbox_model_runtime::MessagePart::ToolCall(replayed_malformed) =
            &rendered[0].parts[2]
        else {
            panic!("malformed proposal remains in the assistant group");
        };
        assert_eq!(
            replayed_malformed.arguments_json,
            r#"{"signalbox_invalid_arguments":true}"#
        );
        let signalbox_model_runtime::MessagePart::ToolCall(replayed_scalar) = &rendered[0].parts[4]
        else {
            panic!("scalar proposal remains in the assistant group");
        };
        assert_eq!(
            replayed_scalar.arguments_json,
            r#"{"signalbox_invalid_arguments":true}"#
        );
        assert_eq!(malformed.arguments().as_str(), "{\"timezone\":");
        assert_eq!(scalar.arguments().as_str(), "7");
        let signalbox_model_runtime::MessagePart::ToolCall(replayed_deep) = &rendered[0].parts[5]
        else {
            panic!("deep valid proposal remains in the assistant group");
        };
        assert_eq!(replayed_deep.arguments_json, deep_arguments);
        assert_eq!(
            rendered[1].role,
            signalbox_model_runtime::ConversationRole::User
        );
        assert_eq!(rendered[1].parts.len(), 3);
    }

    /// INV-026: the application-owned dispatch permit is released exactly
    /// when the runtime first reports that provider acceptance is possible.
    #[test]
    fn inv026_send_commenced_releases_acceptance_callback_once() {
        let release_count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&release_count);
        let mut sink = AcceptanceObservations {
            expected_correlation: call(),
            correlation_mismatch: false,
            acceptance_possible: Some(move || {
                callback_count.fetch_add(1, Ordering::SeqCst);
            }),
            text_deltas: None,
            observations: Vec::new(),
        };

        sink.observe(Observation {
            correlation: call(),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new("model-exact")),
        });
        assert_eq!(release_count.load(Ordering::SeqCst), 0);

        sink.observe(Observation {
            correlation: call(),
            fact: ObservationFact::SendCommenced,
        });
        sink.observe(Observation {
            correlation: call(),
            fact: ObservationFact::SendCommenced,
        });

        assert_eq!(release_count.load(Ordering::SeqCst), 1);
        assert_eq!(sink.observations.len(), 3);
    }

    /// INV-026: cross-wired acceptance evidence cannot release another
    /// attempt's dispatch/stop gate.
    #[test]
    fn inv026_cross_wired_send_commenced_retains_acceptance_callback() {
        let release_count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&release_count);
        let mut sink = AcceptanceObservations {
            expected_correlation: call(),
            correlation_mismatch: false,
            acceptance_possible: Some(move || {
                callback_count.fetch_add(1, Ordering::SeqCst);
            }),
            text_deltas: None,
            observations: Vec::new(),
        };

        sink.observe(Observation {
            correlation: ModelCallId::from_uuid(Uuid::from_u128(2)),
            fact: ObservationFact::SendCommenced,
        });

        assert_eq!(release_count.load(Ordering::SeqCst), 0);
        assert!(sink.correlation_mismatch);
        assert!(sink.acceptance_possible.is_some());
    }

    #[derive(Clone, Default)]
    struct RecordedTextDeltas(Arc<Mutex<Vec<ProviderTextDelta>>>);

    impl ProviderTextDeltaSink for RecordedTextDeltas {
        fn publish(&self, delta: ProviderTextDelta) {
            self.0
                .lock()
                .expect("the fixture delta recorder is not poisoned")
                .push(delta);
        }
    }

    /// INV-035: correctly correlated text crosses the bridge exactly as the
    /// adapter sink supplied it, while cross-wired text stays on the evidence
    /// path and never reaches presentation delivery.
    #[test]
    fn inv035_text_delta_delivery_is_additive_and_correlation_checked() {
        let expected_call = call();
        let expected_session = SessionId::from_uuid(Uuid::from_u128(10));
        let expected_turn = TurnId::from_uuid(Uuid::from_u128(11));
        let expected_part_index = 3;
        let expected_text = String::from("already [redacted]");
        let recorded = RecordedTextDeltas::default();
        let mut sink = AcceptanceObservations {
            expected_correlation: expected_call,
            correlation_mismatch: false,
            acceptance_possible: Some(|| {}),
            text_deltas: Some(ProviderTextDeltaContext {
                session: expected_session,
                turn: expected_turn,
                call: expected_call,
                sink: Arc::new(recorded.clone()),
            }),
            observations: Vec::new(),
        };
        let mismatched = Observation {
            correlation: ModelCallId::from_uuid(Uuid::from_u128(2)),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: String::from("must not publish"),
            },
        };
        let redacted = Observation {
            correlation: expected_call,
            fact: ObservationFact::TextDelta {
                index: expected_part_index,
                text: expected_text.clone(),
            },
        };

        sink.observe(mismatched.clone());
        sink.observe(redacted.clone());

        assert!(sink.correlation_mismatch);
        assert_eq!(sink.observations, vec![mismatched, redacted]);
        assert_eq!(
            *recorded
                .0
                .lock()
                .expect("the fixture delta recorder is not poisoned"),
            vec![ProviderTextDelta::new(
                expected_session,
                expected_turn,
                expected_call,
                expected_part_index,
                expected_text,
            )]
        );
    }

    #[test]
    fn cloned_provider_text_deltas_share_the_text_allocation() {
        let delta = ProviderTextDelta::new(
            SessionId::from_uuid(Uuid::from_u128(10)),
            TurnId::from_uuid(Uuid::from_u128(11)),
            call(),
            3,
            String::from("shared provider text"),
        );
        let cloned = delta.clone();

        assert!(Arc::ptr_eq(&delta.text, &cloned.text));
        assert_eq!(cloned.text(), delta.text());
    }

    /// S02 / INV-014 / INV-025: runtime terminal evidence maps to the exact
    /// physical disposition without retryability or error-string inference.
    #[test]
    fn s02_inv014_inv025_terminal_evidence_classification_is_total() {
        let exchange = ExchangeFacts::default();
        assert_eq!(
            classify_terminal(
                TerminalEvidence::Refused(RefusalEvidence {
                    exchange: exchange.clone(),
                    message_id: None,
                    reported_model: None,
                    content: Vec::new(),
                    usage: TokenUsage::unreported(),
                }),
                &[],
                &configured("model-exact"),
            )
            .expect("typed refusal evidence is supported")
            .observation,
            ModelCallTerminalObservation::Refused
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::ProviderError(ProviderErrorEvidence {
                    exchange: exchange.clone(),
                    reported_model: None,
                    kind: ProviderErrorKind::RateLimited,
                    native: NativeErrorFacts::default(),
                    usage: TokenUsage::unreported(),
                }),
                &[],
                &configured("model-exact"),
            )
            .expect("typed provider-error evidence is supported")
            .observation,
            ModelCallTerminalObservation::KnownFailed
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::CancellationConfirmed(CancellationConfirmedEvidence {
                    exchange: exchange.clone(),
                    reported_model: None,
                    native: NativeErrorFacts::default(),
                }),
                &[],
                &configured("model-exact"),
            )
            .expect("typed cancellation evidence is supported")
            .observation,
            ModelCallTerminalObservation::Cancelled
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::ConnectFailed(TransportFacts {
                        detail: String::from("safe typed fixture"),
                    }),
                }),
                &[],
                &configured("model-exact"),
            )
            .expect("typed non-acceptance evidence is supported")
            .observation,
            ModelCallTerminalObservation::KnownFailed
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::CancelledBeforeSend,
                }),
                &[],
                &configured("model-exact"),
            )
            .expect("pre-send cancellation evidence is supported")
            .observation,
            ModelCallTerminalObservation::Cancelled
        );
        assert_eq!(
            classify_terminal(
                TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                    cause: LossCause::TransportFailed(TransportFacts {
                        detail: String::from("safe typed fixture"),
                    }),
                    exchange,
                    reported_model: None,
                    finish_reported: None,
                    usage: TokenUsage::unreported(),
                }),
                &[],
                &configured("model-exact"),
            )
            .expect("typed boundary-loss evidence is supported")
            .observation,
            ModelCallTerminalObservation::Ambiguous
        );
    }

    /// S02 / INV-014: only exact text from a matching reported target becomes
    /// assistant content; empty blocks create no invalid empty entry.
    #[test]
    fn s02_inv014_matching_completion_preserves_text_parts() {
        assert_eq!(
            classify_terminal(
                completion(
                    "model-exact",
                    vec![
                        AssistantPart::Text(String::from("first")),
                        AssistantPart::Text(String::new()),
                        AssistantPart::Text(String::from("second")),
                    ],
                ),
                &[],
                &configured("model-exact"),
            )
            .expect("text-only completion is supported")
            .observation,
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    signalbox_domain::AssistantText::try_new(String::from("first"))
                        .expect("fixture text is admitted"),
                    signalbox_domain::AssistantText::try_new(String::from("second"))
                        .expect("fixture text is admitted"),
                ],
            }
        );
    }

    /// S10 / INV-002 / INV-005: runtime-native tool calls become ordered,
    /// normalized domain proposals without retaining provider identifiers.
    #[test]
    fn s10_inv002_inv005_tool_completion_crosses_as_provider_neutral_proposals() {
        let classified = classify_terminal(
            tool_completion("model-exact"),
            &[],
            &configured("model-exact"),
        )
        .expect("tool-use completion is supported");
        let ModelCallTerminalObservation::CompletedWithTools { response } = classified.observation
        else {
            panic!("tool-use finish produces a same-turn tool round");
        };
        assert_eq!(response.parts().len(), 2);
        assert!(matches!(
            &response.parts()[0],
            signalbox_domain::AssistantResponsePart::Text(text)
                if text.as_str() == "checking"
        ));
        assert!(matches!(
            &response.parts()[1],
            signalbox_domain::AssistantResponsePart::ToolCall(proposal)
                if proposal.name().as_str() == "current_time"
                    && proposal.arguments().as_str() == r#"{"timezone":"UTC"}"#
        ));
    }

    /// S02 / INV-014: a Claude 5-family tool completion carrying the
    /// omitted-display empty thinking part classifies as a tool round — the
    /// empty part is dropped like an empty text block instead of failing the
    /// whole legitimate completion closed.
    #[test]
    fn s02_inv014_empty_thinking_part_is_dropped_from_a_tool_completion() {
        let classified = classify_terminal(
            completion_with_finish(
                "model-exact",
                CompletionFinish::ToolUse,
                vec![
                    AssistantPart::Thinking {
                        text: String::new(),
                        signature: Some(String::from("sig_synthetic_1")),
                    },
                    AssistantPart::ToolCall(ToolCallProposal {
                        id: ToolCallId::new("provider-call-opaque"),
                        name: ToolName::new("current_time"),
                        arguments_json: String::from("{}"),
                    }),
                ],
            ),
            &[],
            &configured("model-exact"),
        )
        .expect("an empty thinking part must not fail a tool completion closed");
        let ModelCallTerminalObservation::CompletedWithTools { response } = classified.observation
        else {
            panic!("the tool completion still yields its same-turn tool round");
        };
        assert_eq!(response.parts().len(), 1);
        assert!(matches!(
            &response.parts()[0],
            signalbox_domain::AssistantResponsePart::ToolCall(proposal)
                if proposal.name().as_str() == "current_time"
        ));
    }

    /// S02 / INV-014: thinking with actual text still fails the bridge
    /// closed — dropping it would silently erase response material for which
    /// no durable semantic representation exists.
    #[test]
    fn s02_inv014_nonempty_thinking_part_still_fails_closed() {
        let outcome = classify_terminal(
            completion(
                "model-exact",
                vec![AssistantPart::Thinking {
                    text: String::from("visible reasoning"),
                    signature: Some(String::from("sig_synthetic_1")),
                }],
            ),
            &[],
            &configured("model-exact"),
        );
        assert!(matches!(
            outcome,
            Err(failure)
                if matches!(
                    failure.error,
                    RuntimeModelCallProviderError::UnsupportedCompletionMaterial
                )
        ));
    }

    /// S10 / INV-002: tool-call content and the `ToolUse` finish reason must
    /// agree before either terminal completion observation is constructed.
    #[test]
    fn s10_inv002_mismatched_tool_finish_is_known_failed() {
        assert_eq!(
            classify_terminal(
                completion(
                    "model-exact",
                    vec![AssistantPart::ToolCall(ToolCallProposal {
                        id: ToolCallId::new("provider-call-1"),
                        name: ToolName::new("current_time"),
                        arguments_json: String::from("{}"),
                    })],
                ),
                &[],
                &configured("model-exact"),
            )
            .expect("mismatched finish still classifies")
            .observation,
            ModelCallTerminalObservation::KnownFailed,
            "tool calls without a ToolUse finish are not an admitted batch"
        );
        assert_eq!(
            classify_terminal(
                completion_with_finish(
                    "model-exact",
                    CompletionFinish::ToolUse,
                    vec![AssistantPart::Text(String::from("no call"))],
                ),
                &[],
                &configured("model-exact"),
            )
            .expect("mismatched finish still classifies")
            .observation,
            ModelCallTerminalObservation::KnownFailed,
            "a ToolUse finish without a tool call is not an ordinary completion"
        );
    }

    #[test]
    fn checked_tool_json_decoding_is_stack_guarded_beyond_serde_default_depth() {
        let depth = 512;
        let json = format!(r#"{{"nested":{}{}}}"#, "[".repeat(depth), "]".repeat(depth));

        assert!(
            decode_checked_raw_json(&json)
                .expect("checked bounded JSON remains decodable")
                .get()
                .starts_with('{')
        );
    }

    /// INV-014: malformed tool proposals are terminal known failures, so the
    /// provider operation cannot remain durably in flight.
    #[test]
    fn inv014_invalid_tool_proposals_close_as_known_failure() {
        let invalid_name = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("model-exact")),
            finish: CompletionFinish::ToolUse,
            content: vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("provider-call-opaque"),
                name: ToolName::new("bad name"),
                arguments_json: String::from("{}"),
            })],
            usage: TokenUsage::unreported(),
        });
        let oversized_arguments = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("model-exact")),
            finish: CompletionFinish::ToolUse,
            content: vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("provider-call-opaque"),
                name: ToolName::new("current_time"),
                arguments_json: "x".repeat(1024 * 1024 + 1),
            })],
            usage: TokenUsage::unreported(),
        });
        let nul_arguments = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("model-exact")),
            finish: CompletionFinish::ToolUse,
            content: vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("provider-call-opaque"),
                name: ToolName::new("current_time"),
                arguments_json: String::from("{\"zone\":\"UTC\\u0000\"}\0"),
            })],
            usage: TokenUsage::unreported(),
        });
        let mismatched_finish = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("model-exact")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::ToolCall(ToolCallProposal {
                id: ToolCallId::new("provider-call-opaque"),
                name: ToolName::new("current_time"),
                arguments_json: String::from("{}"),
            })],
            usage: TokenUsage::unreported(),
        });

        assert_invalid_tool_proposal_closes(invalid_name);
        assert_invalid_tool_proposal_closes(oversized_arguments);
        assert_invalid_tool_proposal_closes(nul_arguments);
        assert_invalid_tool_proposal_closes(mismatched_finish);
    }

    /// INV-014: either early or terminal evidence of a *different lineage*
    /// prevents response material from becoming authoritative, and the
    /// substitution is its own recorded outcome rather than an ordinary
    /// provider failure.
    #[test]
    fn inv014_cross_model_substitution_precedes_completion() {
        let early = vec![Observation {
            correlation: call(),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                "claude-opus-4-8",
            )),
        }];
        assert_eq!(
            classify_terminal(
                completion(
                    "claude-haiku-4-5",
                    vec![AssistantPart::Text(String::from("hidden"))],
                ),
                &early,
                &configured("claude-haiku-4-5"),
            )
            .expect_err("a substituted lineage must fail closed"),
            super::ClassificationFailure {
                error: super::RuntimeModelCallProviderError::ProviderTargetSubstituted,
                served_target: Some(String::from("claude-opus-4-8")),
            }
        );
        assert_eq!(
            classify_terminal(
                completion(
                    "claude-opus-4-8",
                    vec![AssistantPart::Text(String::from("hidden"))],
                ),
                &[],
                &configured("claude-haiku-4-5"),
            )
            .expect_err("a substituted lineage must fail closed"),
            super::ClassificationFailure {
                error: super::RuntimeModelCallProviderError::ProviderTargetSubstituted,
                served_target: Some(String::from("claude-opus-4-8")),
            }
        );
    }

    /// S20 / INV-014: an alias resolved to its own canonical dated form is
    /// the same logical target. The exchange completes, and the concrete
    /// identity that actually served it is retained as sanitized evidence.
    ///
    /// This is the regression pin for the live wedge: before the
    /// normalization law, a configured undated alias whose response echoed
    /// the dated identity failed the adapter stage closed, terminalized the
    /// call ambiguously, and stopped the daemon.
    #[test]
    fn s20_inv014_alias_resolved_to_its_dated_form_is_the_same_target() {
        let early = vec![Observation {
            correlation: call(),
            fact: ObservationFact::ProviderModelReported(ProviderReportedModel::new(
                "claude-haiku-4-5-20251001",
            )),
        }];

        let classified = classify_terminal(
            completion(
                "claude-haiku-4-5-20251001",
                vec![AssistantPart::Text(String::from("hello"))],
            ),
            &early,
            &configured("claude-haiku-4-5"),
        )
        .expect("an alias made concrete is the configured target");

        assert_eq!(
            classified.observation,
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    signalbox_domain::AssistantText::try_new(String::from("hello"))
                        .expect("fixture text is admitted"),
                ],
            }
        );
        assert_eq!(classified.cause, super::ModelCallCauseCode::Completed);
        assert_eq!(
            classified.concrete_target.as_deref(),
            Some("claude-haiku-4-5-20251001"),
            "the concrete identity that served the exchange is retained as evidence"
        );
    }

    /// S20 / INV-014: an exactly matching identity needs no normalization
    /// record, so nothing is manufactured for it.
    #[test]
    fn s20_inv014_exact_identity_records_no_concretion() {
        let classified = classify_terminal(
            completion(
                "claude-haiku-4-5",
                vec![AssistantPart::Text(String::from("hello"))],
            ),
            &[],
            &configured("claude-haiku-4-5"),
        )
        .expect("an exact identity is the configured target");

        assert_eq!(classified.concrete_target, None);
    }

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct RelationRow {
        configured: &'static str,
        reported: &'static str,
        relation: String,
    }

    /// Renders one relation row per configured/reported pair, in the given
    /// order.
    fn relation_rows(pairs: &[(&'static str, &'static str)]) -> Vec<RelationRow> {
        pairs
            .iter()
            .map(|(configured_spelling, reported_spelling)| RelationRow {
                configured: configured_spelling,
                reported: reported_spelling,
                relation: format!(
                    "{:?}",
                    super::relate_provider_target(
                        &configured(configured_spelling),
                        &reported(reported_spelling),
                    )
                ),
            })
            .collect()
    }

    /// S20 / INV-014: the discriminator between an alias made concrete and a
    /// substituted lineage, stated as a table.
    #[test]
    fn s20_inv014_provider_target_relation_rule_is_stated_by_example() {
        let rows = relation_rows(&[
            ("claude-haiku-4-5", "claude-haiku-4-5"),
            ("claude-haiku-4-5", "claude-haiku-4-5-20251001"),
            ("claude-haiku-4-5", "claude-haiku-4-5-2025-10-01"),
            ("claude-haiku-4-5", "claude-opus-4-8"),
            ("claude-haiku-4-5", "claude-haiku-4-5-fast"),
            ("claude-haiku-4-5", "claude-haiku-4-5-2025100"),
            ("claude-haiku-4-5", "claude-haiku-4-5-202510012"),
            ("claude-opus-4", "claude-opus-4-5"),
            ("claude-opus-4-5", "claude-opus-4-5-20251101"),
            ("claude-opus-4-5-20251101", "claude-opus-4-5"),
            ("", "claude-haiku-4-5-20251001"),
        ]);

        expect![[r#"
            ┌──────────────────────────┬─────────────────────────────┬──────────────────┐
            │ configured               │ reported                    │ relation         │
            ├──────────────────────────┼─────────────────────────────┼──────────────────┤
            │ claude-haiku-4-5         │ claude-haiku-4-5            │ Exact            │
            │ claude-haiku-4-5         │ claude-haiku-4-5-20251001   │ AliasConcretion  │
            │ claude-haiku-4-5         │ claude-haiku-4-5-2025-10-01 │ AliasConcretion  │
            │ claude-haiku-4-5         │ claude-opus-4-8             │ DifferentLineage │
            │ claude-haiku-4-5         │ claude-haiku-4-5-fast       │ DifferentLineage │
            │ claude-haiku-4-5         │ claude-haiku-4-5-2025100    │ DifferentLineage │
            │ claude-haiku-4-5         │ claude-haiku-4-5-202510012  │ DifferentLineage │
            │ claude-opus-4            │ claude-opus-4-5             │ DifferentLineage │
            │ claude-opus-4-5          │ claude-opus-4-5-20251101    │ AliasConcretion  │
            │ claude-opus-4-5-20251101 │ claude-opus-4-5             │ DifferentLineage │
            │ ""                       │ claude-haiku-4-5-20251001   │ DifferentLineage │
            └──────────────────────────┴─────────────────────────────┴──────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct CauseRow {
        outcome: &'static str,
        cause_code: &'static str,
    }

    /// Renders one cause row per classifiable outcome, in the given order.
    ///
    /// Every evidence value here classifies, so the row builder reads the
    /// cause directly; the fail-closed cause codes are asserted separately.
    fn cause_rows(outcomes: Vec<(&'static str, TerminalEvidence)>) -> Vec<CauseRow> {
        outcomes
            .into_iter()
            .map(|(outcome, evidence)| CauseRow {
                outcome,
                cause_code: classify_terminal(evidence, &[], &configured("model-exact"))
                    .expect("every fixture in this table classifies")
                    .cause
                    .as_str(),
            })
            .collect()
    }

    /// INV-035: every classified outcome carries a stable, sanitized operator
    /// cause token; no provider text, response body, or credential material
    /// can reach one.
    #[test]
    fn inv035_every_classified_outcome_carries_a_stable_sanitized_cause_code() {
        let rows = cause_rows(vec![
            (
                "completed",
                completion("model-exact", vec![AssistantPart::Text(String::from("ok"))]),
            ),
            (
                "refused",
                TerminalEvidence::Refused(RefusalEvidence {
                    exchange: ExchangeFacts::default(),
                    message_id: None,
                    reported_model: None,
                    content: Vec::new(),
                    usage: TokenUsage::unreported(),
                }),
            ),
            (
                "provider_error(credential_rejected)",
                TerminalEvidence::ProviderError(ProviderErrorEvidence {
                    exchange: ExchangeFacts::default(),
                    reported_model: None,
                    kind: ProviderErrorKind::CredentialRejected,
                    native: NativeErrorFacts::default(),
                    usage: TokenUsage::unreported(),
                }),
            ),
            (
                "proven_unsent(connect_failed)",
                TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::ConnectFailed(TransportFacts {
                        detail: String::from("safe typed fixture"),
                    }),
                }),
            ),
            (
                "proven_unsent(cancelled_before_send)",
                TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::CancelledBeforeSend,
                }),
            ),
            (
                "boundary_loss(transport_failed)",
                TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                    cause: LossCause::TransportFailed(TransportFacts {
                        detail: String::from("safe typed fixture"),
                    }),
                    exchange: ExchangeFacts::default(),
                    reported_model: None,
                    finish_reported: None,
                    usage: TokenUsage::unreported(),
                }),
            ),
        ]);

        expect![[r#"
            ┌──────────────────────────────────────┬────────────────────────────────┐
            │ outcome                              │ cause_code                     │
            ├──────────────────────────────────────┼────────────────────────────────┤
            │ completed                            │ completed                      │
            │ refused                              │ provider_refused               │
            │ provider_error(credential_rejected)  │ provider_credential_rejected   │
            │ proven_unsent(connect_failed)        │ connect_failed                 │
            │ proven_unsent(cancelled_before_send) │ cancelled_before_send          │
            │ boundary_loss(transport_failed)      │ boundary_loss_transport_failed │
            └──────────────────────────────────────┴────────────────────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }

    /// INV-035: a fail-closed substitution carries the same sanitized cause
    /// vocabulary as a classified outcome.
    #[test]
    fn inv035_substitution_failure_carries_its_sanitized_cause_code() {
        let failure = classify_terminal(
            completion("claude-opus-4-8", vec![]),
            &[],
            &configured("claude-haiku-4-5"),
        )
        .expect_err("a substituted lineage must fail closed");

        assert_eq!(
            failure.error.cause_code().as_str(),
            "provider_target_substituted"
        );
        assert_eq!(failure.served_target.as_deref(), Some("claude-opus-4-8"));
    }

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct PreparationRow {
        failure: &'static str,
        cause_code: &'static str,
    }

    /// Renders one cause row per trustworthy pre-send preparation failure, in
    /// the given order.
    fn preparation_rows(failures: Vec<(&'static str, PreparationFailure)>) -> Vec<PreparationRow> {
        failures
            .into_iter()
            .map(|(failure_name, failure)| PreparationRow {
                failure: failure_name,
                cause_code: super::preparation_failure_cause(&failure).as_str(),
            })
            .collect()
    }

    /// INV-035: a trustworthy pre-send preparation failure — the outcome the
    /// application commits as `KnownFailed` before any provider traffic —
    /// carries the same stable, sanitized cause vocabulary as a terminal
    /// classification, without its adapter-rendered detail text.
    #[test]
    fn inv035_preparation_failures_carry_stable_sanitized_cause_codes() {
        let rows = preparation_rows(vec![
            (
                "unsupported_operation",
                PreparationFailure::UnsupportedOperation {
                    detail: String::from("safe typed fixture"),
                },
            ),
            (
                "credential_unavailable(unmapped)",
                PreparationFailure::CredentialUnavailable {
                    error: CredentialAccessError::new(
                        signalbox_model_runtime::CredentialReference::new("scripted-test"),
                        CredentialAccessFailure::Unmapped,
                    ),
                },
            ),
            (
                "credential_unavailable(unavailable)",
                PreparationFailure::CredentialUnavailable {
                    error: CredentialAccessError::new(
                        signalbox_model_runtime::CredentialReference::new("scripted-test"),
                        CredentialAccessFailure::Unavailable,
                    ),
                },
            ),
            (
                "credential_unavailable(unreadable)",
                PreparationFailure::CredentialUnavailable {
                    error: CredentialAccessError::new(
                        signalbox_model_runtime::CredentialReference::new("scripted-test"),
                        CredentialAccessFailure::Unreadable,
                    ),
                },
            ),
            (
                "credential_unusable",
                PreparationFailure::CredentialUnusable {
                    detail: String::from("safe typed fixture"),
                },
            ),
        ]);

        expect![[r#"
            ┌─────────────────────────────────────┬────────────────────────┐
            │ failure                             │ cause_code             │
            ├─────────────────────────────────────┼────────────────────────┤
            │ unsupported_operation               │ unsupported_operation  │
            │ credential_unavailable(unmapped)    │ credential_unmapped    │
            │ credential_unavailable(unavailable) │ credential_unavailable │
            │ credential_unavailable(unreadable)  │ credential_unreadable  │
            │ credential_unusable                 │ credential_unusable    │
            └─────────────────────────────────────┴────────────────────────┘
        "#]]
        .assert_eq(&table(rows));
    }

    /// INV-035: a hostile provider-reported identity is bounded before it can
    /// reach an operator log line.
    #[test]
    fn inv035_diagnostic_model_identity_is_bounded() {
        let configured = "claude-haiku-4-5";
        let reported = format!("{configured}-{}", "1".repeat(8));
        assert_eq!(
            super::diagnostic_model_identity(&reported).len(),
            reported.len()
        );

        let hostile = "x".repeat(super::DIAGNOSTIC_MODEL_IDENTITY_LIMIT * 4);
        let bounded = super::diagnostic_model_identity(&hostile);
        assert!(bounded.starts_with(&"x".repeat(super::DIAGNOSTIC_MODEL_IDENTITY_LIMIT)));
        assert!(bounded.ends_with("… [truncated]"));
    }

    #[test]
    fn conflicting_runtime_target_meaning_is_rejected() {
        assert_eq!(
            RuntimeModelCatalog::try_from_definitions([
                RuntimeModelDefinition::try_new(target(1), String::from("first"), 64, 200_000)
                    .expect("fixture definition is valid"),
                RuntimeModelDefinition::try_new(target(1), String::from("second"), 64, 200_000)
                    .expect("fixture definition is valid"),
            ]),
            Err(RuntimeModelCatalogError::ConflictingTarget { target: target(1) })
        );
    }
}
