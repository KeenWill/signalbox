//! Bridge from the application-owned model-call port to a Layer-1 runtime.
//!
//! The layer boundary in docs/spec/runtime-substrate.md keeps runtime types
//! out of the domain and application crates. This crate is the outward
//! adapter: it translates one checked application operation, moves the
//! runtime's opaque one-shot capability across durable authorization, and
//! maps typed terminal evidence into the domain dispositions defined in
//! docs/spec/model-call-execution.md. It owns no retry, fallback, lifecycle,
//! or durable state.

mod approval_judge;
mod context_compaction;

pub use approval_judge::{
    ApprovalJudgeModel, ApprovalJudgeModelError, ApprovalJudgeModelRequest,
    ApprovalJudgeModelResult, PreparedApprovalJudgeModelCall, RuntimeApprovalJudgeModel,
    approval_judge_output_contract_text,
};
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
    AnthropicServiceTier as DomainAnthropicServiceTier, AssistantResponsePart, AssistantText,
    AuthorizedModelCall, CodexCliServiceTier as DomainCodexCliServiceTier, ContextFrontierId,
    DelegationOutcome, DelegationOutcomeKind, DelegationOutcomeReason, FastMode as DomainFastMode,
    FrozenModelSelection, ModelCallId, ModelCallTerminalObservation, NormalizedToolArguments,
    OpenAiServiceTier as DomainOpenAiServiceTier, ProviderModelCallFailureCause,
    ProviderReportedTokenUsage, ReasoningLevel as DomainReasoningLevel, ResolvedProviderTarget,
    ServiceTier as DomainServiceTier, SessionId, ToolArgumentsKind,
    ToolCallProposal as DomainToolCallProposal, ToolExecutionErrorKind, ToolName as DomainToolName,
    ToolResultContent, ToolUsingAssistantResponse, TurnAttemptId, TurnId, ValidatedModelSettings,
};
use signalbox_model_runtime::{
    AnthropicServiceTier as RuntimeAnthropicServiceTier, AssistantPart, CancellationSignal,
    CodexCliServiceTier as RuntimeCodexCliServiceTier, CompletionFinish, ConversationMessage,
    ConversationRole, CredentialAccessFailure, CredentialReference, DeliveryMode,
    FastMode as RuntimeFastMode, LossCause, MessagePart, ModelOperation, ModelRuntime,
    ModelSettings, Observation, ObservationFact, ObservationSink,
    OpenAiServiceTier as RuntimeOpenAiServiceTier, PreparationFailure, PreparationOutcome,
    ProviderErrorKind, ProviderReportedModel, ReasoningLevel as RuntimeReasoningLevel,
    RequestedTarget, ResolvedTarget, ServiceTier as RuntimeServiceTier, TerminalEvidence,
    ToolCallId, ToolCallProposal, ToolDefinition, ToolName as RuntimeToolName, ToolResultRecord,
    UnsentCause,
};

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

    /// Shares the already-redacted provider text without copying its allocation.
    pub fn shared_text(&self) -> Arc<str> {
        Arc::clone(&self.text)
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
    fast_target: Option<ResolvedProviderTarget>,
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
        if max_output_tokens > context_window_tokens {
            return Err(RuntimeModelDefinitionError::OutputLimitExceedsContextWindow);
        }
        Ok(Self {
            target,
            provider_model,
            fast_target: None,
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

    /// Declares the separately configured provider target authorized when
    /// this model's validated settings enable mapped fast serving.
    pub const fn with_fast_target(mut self, fast_target: ResolvedProviderTarget) -> Self {
        self.fast_target = Some(fast_target);
        self
    }

    /// Returns the authorized mapped fast target, when one is declared.
    pub const fn fast_target(&self) -> Option<ResolvedProviderTarget> {
        self.fast_target
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
    /// The reserved output alone cannot exceed the declared context window.
    OutputLimitExceedsContextWindow,
}

impl fmt::Display for RuntimeModelDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidProviderModel => "provider model spelling is empty or padded",
            Self::InvalidOutputLimit => "provider output-token limit is zero",
            Self::InvalidContextWindow => "provider context-window limit is zero",
            Self::OutputLimitExceedsContextWindow => {
                "provider output-token limit exceeds its context window"
            }
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
        for definition in by_target.values() {
            if let Some(fast_target) = definition.fast_target
                && !by_target.contains_key(&fast_target)
            {
                return Err(RuntimeModelCatalogError::MissingFastTarget {
                    target: definition.target,
                    fast_target,
                });
            }
        }
        Ok(Self {
            definitions: by_target,
        })
    }

    /// Looks up the exact runtime delivery mapping for a durable target.
    pub fn resolve(&self, target: ResolvedProviderTarget) -> Option<&RuntimeModelDefinition> {
        self.definitions.get(&target)
    }

    /// Resolves the exact serving definition selected by validated fast mode.
    pub fn effective_definition<'catalog>(
        &'catalog self,
        definition: &'catalog RuntimeModelDefinition,
        fast_mode: DomainFastMode,
    ) -> Option<&'catalog RuntimeModelDefinition> {
        Some(match (fast_mode, definition.fast_target) {
            (DomainFastMode::Enabled, Some(target)) => self.resolve(target)?,
            (DomainFastMode::Disabled, _) | (DomainFastMode::Enabled, None) => definition,
        })
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
    /// A mapped fast target has no runtime delivery definition.
    MissingFastTarget {
        /// Source target declaring mapped fast serving.
        target: ResolvedProviderTarget,
        /// Missing authorized fast target.
        fast_target: ResolvedProviderTarget,
    },
}

impl fmt::Display for RuntimeModelCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ConflictingTarget { .. } => "runtime model catalog contains a conflicting target",
            Self::MissingFastTarget { .. } => {
                "runtime model catalog contains a missing mapped fast target"
            }
        })
    }
}

impl Error for RuntimeModelCatalogError {}

fn runtime_delivery_definitions(
    models: &RuntimeModelCatalog,
    target: ResolvedProviderTarget,
    fast_mode: DomainFastMode,
) -> Option<(&RuntimeModelDefinition, &RuntimeModelDefinition)> {
    let selected = models.resolve(target)?;
    let serving = models.effective_definition(selected, fast_mode)?;
    Some((selected, serving))
}

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

/// Declares the runtime-owned model-call telemetry token vocabulary once.
macro_rules! model_call_cause_tokens {
    ($( $variant:ident => $token:literal ),+ $(,)?) => {
        /// One closed, content-free token admitted to model-call telemetry.
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum ModelCallCauseToken {
            $(
                #[doc = concat!("The `", $token, "` cause token.")]
                $variant,
            )+
        }

        impl ModelCallCauseToken {
            /// Parses only a token declared by the runtime-owned vocabulary.
            pub fn parse(value: &str) -> Option<Self> {
                match value {
                    $($token => Some(Self::$variant),)+
                    _ => None,
                }
            }

            /// Returns the fixed operator-facing spelling.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $token,)+
                }
            }
        }
    };
}

model_call_cause_tokens! {
    Completed => "completed",
    ProviderRefused => "provider_refused",
    ProviderCredentialRejected => "provider_credential_rejected",
    ProviderPermissionDenied => "provider_permission_denied",
    ProviderInvalidRequest => "provider_invalid_request",
    ProviderTargetNotFound => "provider_target_not_found",
    ProviderRequestTooLarge => "provider_request_too_large",
    ProviderRateLimited => "provider_rate_limited",
    ProviderQuotaExhausted => "provider_quota_exhausted",
    ProviderOverloaded => "provider_overloaded",
    ProviderInternal => "provider_internal",
    ProviderUnrecognizedError => "provider_unrecognized_error",
    ProviderCancellationConfirmed => "provider_cancellation_confirmed",
    CancelledBeforeSend => "cancelled_before_send",
    ConnectFailed => "connect_failed",
    SendIncompleteProvenUnacceptable => "send_incomplete_proven_unacceptable",
    BoundaryLossCancellationRequested => "boundary_loss_cancellation_requested",
    BoundaryLossTimedOut => "boundary_loss_timed_out",
    BoundaryLossTransportFailed => "boundary_loss_transport_failed",
    BoundaryLossResponseBodyLost => "boundary_loss_response_body_lost",
    BoundaryLossResponseUnintelligible => "boundary_loss_response_unintelligible",
    BoundaryLossUnexpectedHttpStatus => "boundary_loss_unexpected_http_status",
    BoundaryLossStreamIncomplete => "boundary_loss_stream_incomplete",
    BoundaryLossStreamProtocolViolation => "boundary_loss_stream_protocol_violation",
    UnsupportedOperation => "unsupported_operation",
    CredentialUnmapped => "credential_unmapped",
    CredentialUnavailable => "credential_unavailable",
    CredentialUnreadable => "credential_unreadable",
    CredentialUnusable => "credential_unusable",
    ProviderTargetSubstituted => "provider_target_substituted",
    UnrepresentableToolMaterial => "unrepresentable_tool_material",
    FinishContradictsContent => "finish_contradicts_content",
    UnconfiguredTarget => "unconfigured_target",
    PreparationDefect => "preparation_defect",
    CorrelationMismatch => "correlation_mismatch",
    AuthorizationMismatch => "authorization_mismatch",
    ObservationCorrelationMismatch => "observation_correlation_mismatch",
    UnsupportedCompletionMaterial => "unsupported_completion_material",
    InvalidAssistantText => "invalid_assistant_text",
    InvalidToolSchema => "invalid_tool_schema",
    InvalidToolProposal => "invalid_tool_proposal",
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
        self.token().as_str()
    }

    const fn token(self) -> ModelCallCauseToken {
        match self {
            Self::CancellationRequested => ModelCallCauseToken::BoundaryLossCancellationRequested,
            Self::TimedOut => ModelCallCauseToken::BoundaryLossTimedOut,
            Self::TransportFailed => ModelCallCauseToken::BoundaryLossTransportFailed,
            Self::ResponseBodyLost => ModelCallCauseToken::BoundaryLossResponseBodyLost,
            Self::ResponseUnintelligible => ModelCallCauseToken::BoundaryLossResponseUnintelligible,
            Self::UnexpectedHttpStatus => ModelCallCauseToken::BoundaryLossUnexpectedHttpStatus,
            Self::StreamEndedWithoutTerminalMarker => {
                ModelCallCauseToken::BoundaryLossStreamIncomplete
            }
            Self::StreamProtocolViolation => {
                ModelCallCauseToken::BoundaryLossStreamProtocolViolation
            }
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
        self.token().as_str()
    }

    /// Projects this cause through a compiler-checked closed token.
    pub const fn token(self) -> ModelCallCauseToken {
        match self {
            Self::Completed => ModelCallCauseToken::Completed,
            Self::Refused => ModelCallCauseToken::ProviderRefused,
            Self::ProviderError(kind) => provider_error_token(kind),
            Self::CancellationConfirmed => ModelCallCauseToken::ProviderCancellationConfirmed,
            Self::CancelledBeforeSend => ModelCallCauseToken::CancelledBeforeSend,
            Self::ConnectFailed => ModelCallCauseToken::ConnectFailed,
            Self::SendIncompleteProvenUnacceptable => {
                ModelCallCauseToken::SendIncompleteProvenUnacceptable
            }
            Self::BoundaryLoss(code) => code.token(),
            Self::UnsupportedOperation => ModelCallCauseToken::UnsupportedOperation,
            Self::CredentialUnavailable(code) => code.token(),
            Self::CredentialUnusable => ModelCallCauseToken::CredentialUnusable,
            Self::ProviderTargetSubstituted => ModelCallCauseToken::ProviderTargetSubstituted,
            Self::UnrepresentableToolMaterial => ModelCallCauseToken::UnrepresentableToolMaterial,
            Self::FinishContradictsContent => ModelCallCauseToken::FinishContradictsContent,
            Self::UnconfiguredTarget => ModelCallCauseToken::UnconfiguredTarget,
            Self::PreparationDefect => ModelCallCauseToken::PreparationDefect,
            Self::CorrelationMismatch => ModelCallCauseToken::CorrelationMismatch,
            Self::AuthorizationMismatch => ModelCallCauseToken::AuthorizationMismatch,
            Self::ObservationCorrelationMismatch => {
                ModelCallCauseToken::ObservationCorrelationMismatch
            }
            Self::UnsupportedCompletionMaterial => {
                ModelCallCauseToken::UnsupportedCompletionMaterial
            }
            Self::InvalidAssistantText => ModelCallCauseToken::InvalidAssistantText,
            Self::InvalidToolSchema => ModelCallCauseToken::InvalidToolSchema,
            Self::InvalidToolProposal => ModelCallCauseToken::InvalidToolProposal,
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
        self.token().as_str()
    }

    const fn token(self) -> ModelCallCauseToken {
        match self {
            Self::Unmapped => ModelCallCauseToken::CredentialUnmapped,
            Self::Unavailable => ModelCallCauseToken::CredentialUnavailable,
            Self::Unreadable => ModelCallCauseToken::CredentialUnreadable,
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

/// Maps the adapter's exhaustive error taxonomy to the closed domain
/// classification retained beyond this bridge.
const fn provider_failure_cause(kind: ProviderErrorKind) -> ProviderModelCallFailureCause {
    match kind {
        ProviderErrorKind::CredentialRejected => ProviderModelCallFailureCause::CredentialRejected,
        ProviderErrorKind::PermissionDenied => ProviderModelCallFailureCause::PermissionDenied,
        ProviderErrorKind::InvalidRequest => ProviderModelCallFailureCause::InvalidRequest,
        ProviderErrorKind::TargetNotFound => ProviderModelCallFailureCause::TargetNotFound,
        ProviderErrorKind::RequestTooLarge => ProviderModelCallFailureCause::RequestTooLarge,
        ProviderErrorKind::RateLimited => ProviderModelCallFailureCause::RateLimited,
        ProviderErrorKind::QuotaExhausted => ProviderModelCallFailureCause::QuotaExhausted,
        ProviderErrorKind::Overloaded => ProviderModelCallFailureCause::Overloaded,
        ProviderErrorKind::ProviderInternal => ProviderModelCallFailureCause::ProviderInternal,
        ProviderErrorKind::Unrecognized => ProviderModelCallFailureCause::Unrecognized,
    }
}

/// The stable token for one runtime provider-error classification.
///
/// Kept as an exhaustive `match` so a new `ProviderErrorKind` cannot reach
/// operator telemetry without a deliberate token.
const fn provider_error_token(kind: ProviderErrorKind) -> ModelCallCauseToken {
    match kind {
        ProviderErrorKind::CredentialRejected => ModelCallCauseToken::ProviderCredentialRejected,
        ProviderErrorKind::PermissionDenied => ModelCallCauseToken::ProviderPermissionDenied,
        ProviderErrorKind::InvalidRequest => ModelCallCauseToken::ProviderInvalidRequest,
        ProviderErrorKind::TargetNotFound => ModelCallCauseToken::ProviderTargetNotFound,
        ProviderErrorKind::RequestTooLarge => ModelCallCauseToken::ProviderRequestTooLarge,
        ProviderErrorKind::RateLimited => ModelCallCauseToken::ProviderRateLimited,
        ProviderErrorKind::QuotaExhausted => ModelCallCauseToken::ProviderQuotaExhausted,
        ProviderErrorKind::Overloaded => ModelCallCauseToken::ProviderOverloaded,
        ProviderErrorKind::ProviderInternal => ModelCallCauseToken::ProviderInternal,
        ProviderErrorKind::Unrecognized => ModelCallCauseToken::ProviderUnrecognizedError,
    }
}

/// Bounds a provider-reported identity before it reaches operator telemetry.
///
/// The provider controls the reported spelling, so the diagnostic projection
/// is truncated to the configured byte limit on a character boundary. The
/// value is already credential-redacted by the adapter
/// (docs/spec/runtime-substrate.md); this policy keeps a hostile length from
/// reaching a log line when bounded.
fn diagnostic_model_identity(reported: &str, limit: Option<usize>) -> String {
    let Some(limit) = limit else {
        return reported.to_owned();
    };
    if reported.len() <= limit {
        return reported.to_owned();
    }
    let mut boundary = limit;
    while boundary > 0 && !reported.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut bounded = String::from(reported.get(..boundary).unwrap_or_default());
    bounded.push_str("… [truncated]");
    bounded
}

/// Aggregate identities admitted to model-call operator telemetry.
///
/// All three values are daemon-minted and contain no model or user content.
#[derive(Clone, Copy)]
struct ModelCallTelemetry {
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    call: ModelCallId,
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
    /// (docs/spec/runtime-substrate.md) says only
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
    diagnostic_model_identity_limit: Option<usize>,
}

struct AcceptanceObservations<AcceptancePossible, Correlation> {
    expected_correlation: Correlation,
    correlation_mismatch: bool,
    acceptance_possible: Option<AcceptancePossible>,
    telemetry: ModelCallTelemetry,
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
            report_model_call_dispatch(self.telemetry);
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
    pub fn new(
        runtime: R,
        models: RuntimeModelCatalog,
        diagnostic_model_identity_limit: Option<usize>,
    ) -> Self {
        Self {
            runtime: Arc::new(runtime),
            models,
            text_deltas: Arc::new(DiscardProviderTextDeltas),
            diagnostic_model_identity_limit,
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
            diagnostic_model_identity_limit: self.diagnostic_model_identity_limit,
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
}

impl fmt::Display for RuntimeInputTokenCountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("model input token estimation failed")
    }
}

impl Error for RuntimeInputTokenCountError {}

impl ClassifyOperatorFailure for RuntimeInputTokenCountError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::UnconfiguredTarget | Self::InvalidToolSchema | Self::CorrelationMismatch => {
                OperatorFailureClass::CallerOrHubBug
            }
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::UnconfiguredTarget => "model_input_count_unconfigured_target",
            Self::InvalidToolSchema => "model_input_count_invalid_tool_schema",
            Self::CorrelationMismatch => "model_input_count_correlation_mismatch",
        }
    }
}

fn runtime_model_settings(
    max_output_tokens: u32,
    validated: ValidatedModelSettings,
) -> ModelSettings {
    let effective = validated.effective();
    let mut settings = ModelSettings::new(max_output_tokens);
    settings.reasoning_level = effective.reasoning_level().map(runtime_reasoning_level);
    settings.fast_mode = runtime_fast_mode(effective.fast_mode());
    settings.service_tier = effective.service_tier().map(runtime_service_tier);
    settings
}

const fn runtime_reasoning_level(value: DomainReasoningLevel) -> RuntimeReasoningLevel {
    match value {
        DomainReasoningLevel::None => RuntimeReasoningLevel::None,
        DomainReasoningLevel::Minimal => RuntimeReasoningLevel::Minimal,
        DomainReasoningLevel::Low => RuntimeReasoningLevel::Low,
        DomainReasoningLevel::Medium => RuntimeReasoningLevel::Medium,
        DomainReasoningLevel::High => RuntimeReasoningLevel::High,
        DomainReasoningLevel::XHigh => RuntimeReasoningLevel::XHigh,
        DomainReasoningLevel::Max => RuntimeReasoningLevel::Max,
        DomainReasoningLevel::Ultra => RuntimeReasoningLevel::Ultra,
    }
}

const fn runtime_fast_mode(value: DomainFastMode) -> RuntimeFastMode {
    match value {
        DomainFastMode::Disabled => RuntimeFastMode::Disabled,
        DomainFastMode::Enabled => RuntimeFastMode::Enabled,
    }
}

const fn runtime_service_tier(value: DomainServiceTier) -> RuntimeServiceTier {
    match value {
        DomainServiceTier::Anthropic(value) => RuntimeServiceTier::Anthropic(match value {
            DomainAnthropicServiceTier::Auto => RuntimeAnthropicServiceTier::Auto,
            DomainAnthropicServiceTier::StandardOnly => RuntimeAnthropicServiceTier::StandardOnly,
        }),
        DomainServiceTier::OpenAi(value) => RuntimeServiceTier::OpenAi(match value {
            DomainOpenAiServiceTier::Auto => RuntimeOpenAiServiceTier::Auto,
            DomainOpenAiServiceTier::Default => RuntimeOpenAiServiceTier::Default,
            DomainOpenAiServiceTier::Flex => RuntimeOpenAiServiceTier::Flex,
            DomainOpenAiServiceTier::Scale => RuntimeOpenAiServiceTier::Scale,
            DomainOpenAiServiceTier::Priority => RuntimeOpenAiServiceTier::Priority,
            DomainOpenAiServiceTier::Fast => RuntimeOpenAiServiceTier::Fast,
        }),
        DomainServiceTier::CodexCli(value) => RuntimeServiceTier::CodexCli(match value {
            DomainCodexCliServiceTier::Default => RuntimeCodexCliServiceTier::Default,
            DomainCodexCliServiceTier::Priority => RuntimeCodexCliServiceTier::Priority,
            DomainCodexCliServiceTier::Flex => RuntimeCodexCliServiceTier::Flex,
        }),
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
        let telemetry = ModelCallTelemetry {
            session: request.session(),
            turn: request.turn(),
            attempt: request.attempt(),
            call: correlation,
        };
        let (definition, effective_definition) = runtime_delivery_definitions(
            &self.models,
            call.target(),
            request.model_settings().effective().fast_mode(),
        )
        .ok_or(RuntimeInputTokenCountError::UnconfiguredTarget)?;
        let messages = render_runtime_messages(operation.messages());
        let tools = runtime_tool_definitions(operation.tools()).map_err(|error| {
            report_invalid_runtime_tool_schema(telemetry, &error);
            RuntimeInputTokenCountError::InvalidToolSchema
        })?;
        let mut runtime_operation = ModelOperation::new(
            correlation,
            CredentialReference::new(operation.credential_reference().as_str().to_owned()),
            RequestedTarget::new(render_requested_target(call.selection())),
            ResolvedTarget::new(definition.provider_model().to_owned()),
            messages,
            runtime_model_settings(
                effective_definition.max_output_tokens(),
                request.model_settings(),
            ),
        );
        runtime_operation.system = operation.system_prompt().map(str::to_owned);
        runtime_operation.tools = tools;
        runtime_operation.delivery = DeliveryMode::Streamed;
        classify_runtime_input_count(
            self.runtime
                .count_input_tokens(runtime_operation, CancellationSignal::when(cancellation))
                .await,
            correlation,
        )
    }
}

fn classify_runtime_input_count(
    outcome: signalbox_model_runtime::InputTokenCountOutcome<ModelCallId>,
    correlation: ModelCallId,
) -> Result<ModelCallInputTokenCount, RuntimeInputTokenCountError> {
    match outcome {
        signalbox_model_runtime::InputTokenCountOutcome::Counted {
            correlation: returned,
            input_tokens,
        } if returned == correlation => Ok(ModelCallInputTokenCount::Counted(input_tokens)),
        signalbox_model_runtime::InputTokenCountOutcome::Cancelled {
            correlation: returned,
        } if returned == correlation => Ok(ModelCallInputTokenCount::Cancelled),
        signalbox_model_runtime::InputTokenCountOutcome::Unavailable {
            correlation: returned,
        } if returned == correlation => Ok(ModelCallInputTokenCount::Unavailable),
        signalbox_model_runtime::InputTokenCountOutcome::Failed {
            correlation: returned,
        } if returned == correlation => Ok(ModelCallInputTokenCount::Unavailable),
        signalbox_model_runtime::InputTokenCountOutcome::Counted { .. }
        | signalbox_model_runtime::InputTokenCountOutcome::Cancelled { .. }
        | signalbox_model_runtime::InputTokenCountOutcome::Unavailable { .. }
        | signalbox_model_runtime::InputTokenCountOutcome::Failed { .. } => {
            Err(RuntimeInputTokenCountError::CorrelationMismatch)
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
        let telemetry = ModelCallTelemetry {
            session: request.session(),
            turn: request.turn(),
            attempt: request.attempt(),
            call: correlation,
        };
        let (definition, effective_definition) = runtime_delivery_definitions(
            &self.models,
            call.target(),
            request.model_settings().effective().fast_mode(),
        )
        .ok_or_else(|| {
            fail_closed(
                telemetry,
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
        let tools = runtime_tool_definitions(operation.tools()).map_err(|error| {
            report_invalid_runtime_tool_schema(telemetry, &error);
            fail_closed(
                telemetry,
                RuntimeModelCallProviderError::InvalidToolSchema,
                None,
            )
        })?;
        let selected_target = ResolvedTarget::new(definition.provider_model().to_owned());
        let resolved_target = ResolvedTarget::new(effective_definition.provider_model().to_owned());
        let mut runtime_operation = ModelOperation::new(
            correlation,
            credential,
            RequestedTarget::new(render_requested_target(call.selection())),
            selected_target,
            messages,
            runtime_model_settings(
                effective_definition.max_output_tokens(),
                request.model_settings(),
            ),
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
                require_correlation(telemetry, returned)?;
                Ok(ModelCallCapabilityPreparation::Cancelled)
            }
            PreparationOutcome::Failed {
                correlation: returned,
                failure,
            } => {
                require_correlation(telemetry, returned)?;
                report_preparation_failure(telemetry, &failure);
                Ok(ModelCallCapabilityPreparation::KnownFailure)
            }
            PreparationOutcome::Defect {
                correlation: returned,
                ..
            } => {
                require_correlation(telemetry, returned)?;
                Err(fail_closed(
                    telemetry,
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
        let telemetry = ModelCallTelemetry {
            session: authorized.session(),
            turn: authorized.turn(),
            attempt: authorized.attempt().id(),
            call: correlation,
        };
        if !capability.binding.matches(&authorized) {
            return Err(fail_closed(
                telemetry,
                RuntimeModelCallProviderError::AuthorizationMismatch,
                None,
            ));
        }
        let mut observations = AcceptanceObservations {
            expected_correlation: correlation,
            correlation_mismatch: false,
            acceptance_possible: Some(acceptance_possible),
            telemetry,
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
        require_correlation(telemetry, report.correlation)?;
        if observations.correlation_mismatch {
            return Err(fail_closed(
                telemetry,
                RuntimeModelCallProviderError::ObservationCorrelationMismatch,
                None,
            ));
        }
        let usage = provider_reported_token_usage(&report.evidence);
        let retry_after = match &report.evidence {
            TerminalEvidence::ProviderError(error) => error.exchange.retry_after,
            _ => None,
        };
        let non_acceptance_proven = match &report.evidence {
            TerminalEvidence::ProviderError(error) => error.non_acceptance_proven,
            _ => false,
        };
        let classified = classify_terminal(
            report.evidence,
            &observations.observations,
            &capability.resolved_target,
            self.diagnostic_model_identity_limit,
        )
        .map_err(|failure| {
            fail_closed(telemetry, failure.error, failure.served_target.as_deref())
        })?;
        report_classified_outcome(telemetry, &classified);
        let correlation = authorized.observation_correlation();
        Ok(match classified.cause {
            ModelCallCauseCode::ProviderError(kind) => correlation
                .bind_provider_failure_observation_with_retry_after(
                    provider_failure_cause(kind),
                    usage,
                    retry_after,
                    non_acceptance_proven,
                ),
            _ => correlation.bind_terminal_observation_with_usage(classified.observation, usage),
        })
    }
}

/// Records a provider dispatch only from correctly correlated send evidence.
///
/// This orchestration-layer site is downstream of the adapter observation
/// boundary and fires once at `SendCommenced`, never for work proven unsent.
/// Its fields are daemon-minted identities; provider prose, credentials, and
/// model content are neither inspected nor formatted.
fn report_model_call_dispatch(telemetry: ModelCallTelemetry) {
    tracing::info!(
        session_id = %telemetry.session.as_uuid(),
        turn_id = %telemetry.turn.as_uuid(),
        model_call_id = %telemetry.call.as_uuid(),
        turn_attempt_id = %telemetry.attempt.as_uuid(),
        "model call dispatched"
    );
}

/// Records typed preparation evidence at the provider orchestration boundary.
///
/// Adapter implementations remain telemetry-free: this layer consumes their
/// typed outcome and admits only a closed cause token plus daemon-minted
/// identities. Provider prose, credentials, and conversation content are never
/// formatted or inspected for this event.
fn report_preparation_failure(telemetry: ModelCallTelemetry, failure: &PreparationFailure) {
    tracing::warn!(
        cause_code = preparation_failure_cause(failure).as_str(),
        session_id = %telemetry.session.as_uuid(),
        turn_id = %telemetry.turn.as_uuid(),
        model_call_id = %telemetry.call.as_uuid(),
        "model runtime reported a trustworthy capability-preparation failure"
    );
}

/// Records one fail-closed bridge outcome for operators and returns it.
///
/// Sanitized by construction: daemon-minted session, turn, and call identities,
/// the stable cause token, and — for a substitution — the bounded provider
/// identity that actually served are the only fields, so no provider text,
/// response body, credential material, or user content reaches telemetry.
fn fail_closed(
    telemetry: ModelCallTelemetry,
    error: RuntimeModelCallProviderError,
    served_target: Option<&str>,
) -> RuntimeModelCallProviderError {
    match served_target {
        Some(served_target) => tracing::error!(
            failure_class = ?error.operator_failure_class(),
            cause_code = error.cause_code().as_str(),
            session_id = %telemetry.session.as_uuid(),
            turn_id = %telemetry.turn.as_uuid(),
            model_call_id = %telemetry.call.as_uuid(),
            served_provider_target = served_target,
            "model call failed closed at the runtime bridge"
        ),
        None => tracing::error!(
            failure_class = ?error.operator_failure_class(),
            cause_code = error.cause_code().as_str(),
            session_id = %telemetry.session.as_uuid(),
            turn_id = %telemetry.turn.as_uuid(),
            model_call_id = %telemetry.call.as_uuid(),
            "model call failed closed at the runtime bridge"
        ),
    }
    error
}

/// Records the attributed local schema defect before callers collapse it into
/// their stable coarse outcome. `serde_json` syntax errors carry only grammar
/// and source-position evidence; the rejected schema bytes are never logged.
fn report_invalid_runtime_tool_schema(
    telemetry: ModelCallTelemetry,
    error: &InvalidRuntimeToolSchema,
) {
    tracing::error!(
        failure_class = ?OperatorFailureClass::CallerOrHubBug,
        cause_code = ModelCallCauseCode::InvalidToolSchema.as_str(),
        session_id = %telemetry.session.as_uuid(),
        turn_id = %telemetry.turn.as_uuid(),
        model_call_id = %telemetry.call.as_uuid(),
        tool_name = error.tool_name.as_str(),
        schema_error = %error.source,
        "application tool schema was rejected at the runtime bridge"
    );
}

/// Records one classified terminal outcome for operators.
///
/// The admitted fields are daemon-minted session, turn, and call identities,
/// closed cause tokens, and the already-bounded concrete target; provider
/// response text, credential material, and conversation content stay absent.
fn report_classified_outcome(telemetry: ModelCallTelemetry, classified: &TerminalClassification) {
    if let Some(concrete_target) = &classified.concrete_target {
        tracing::info!(
            session_id = %telemetry.session.as_uuid(),
            turn_id = %telemetry.turn.as_uuid(),
            model_call_id = %telemetry.call.as_uuid(),
            concrete_provider_target = concrete_target.as_str(),
            "provider served the configured target in its concrete dated form"
        );
    }
    match classified.observation {
        ModelCallTerminalObservation::Completed { .. }
        | ModelCallTerminalObservation::CompletedWithTools { .. } => {
            tracing::debug!(
                cause_code = classified.cause.as_str(),
                session_id = %telemetry.session.as_uuid(),
                turn_id = %telemetry.turn.as_uuid(),
                model_call_id = %telemetry.call.as_uuid(),
                "model call completed"
            );
        }
        _ => {
            tracing::warn!(
                cause_code = classified.cause.as_str(),
                session_id = %telemetry.session.as_uuid(),
                turn_id = %telemetry.turn.as_uuid(),
                model_call_id = %telemetry.call.as_uuid(),
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
                rendered.push(ConversationMessage {
                    role: ConversationRole::User,
                    parts: content
                        .parts()
                        .iter()
                        .map(|part| MessagePart::Text(part.as_str().to_owned()))
                        .collect(),
                });
                assistant_call = None;
                collecting_tool_results = false;
            }
            ModelConversationMessage::DelegatedTask { content, .. } => {
                rendered.push(ConversationMessage::user_text(format!(
                    "Signalbox delegated task:\n{}",
                    content.as_str()
                )));
                assistant_call = None;
                collecting_tool_results = false;
            }
            ModelConversationMessage::DelegationMessage {
                sender, content, ..
            } => {
                rendered.push(ConversationMessage::user_text(format!(
                    "Signalbox delegation message from session {}:\n{}",
                    sender.into_uuid(),
                    content.as_str()
                )));
                assistant_call = None;
                collecting_tool_results = false;
            }
            ModelConversationMessage::BackgroundDelegationResult { child, outcome, .. } => {
                let content = match (outcome.kind(), outcome.content()) {
                    (DelegationOutcomeKind::ResultReturned, Some(content)) => format!(
                        "Signalbox background child result from session {}:\n{}",
                        child.into_uuid(),
                        content.as_str()
                    ),
                    _ => format!(
                        "Signalbox background child outcome from session {}: {}",
                        child.into_uuid(),
                        render_delegation_outcome(outcome)
                    ),
                };
                rendered.push(ConversationMessage::user_text(content));
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

/// One application tool definition carried a schema that is not valid JSON.
#[derive(Debug)]
pub struct InvalidRuntimeToolSchema {
    tool_name: String,
    source: serde_json::Error,
}

impl InvalidRuntimeToolSchema {
    /// Returns the safe application tool name whose schema was rejected.
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }
}

impl fmt::Display for InvalidRuntimeToolSchema {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "application tool schema is invalid at the runtime bridge: {}",
            self.tool_name
        )
    }
}

impl Error for InvalidRuntimeToolSchema {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Projects one application tool catalog into the runtime tool definitions
/// every adapter receives.
///
/// This is the single production projection from the daemon registry toward a
/// provider. Conformance tests derive their bridge input through it rather
/// than reproducing it, so a projection that drops or alters a tool is
/// classified instead of being reproduced on both sides of an assertion.
pub fn runtime_tool_definitions(
    definitions: &[signalbox_application::ToolDefinition],
) -> Result<Vec<ToolDefinition>, InvalidRuntimeToolSchema> {
    definitions
        .iter()
        .map(|definition| {
            let schema =
                decode_checked_raw_json(definition.input_schema().as_str()).map_err(|source| {
                    InvalidRuntimeToolSchema {
                        tool_name: definition.name().as_str().to_owned(),
                        source,
                    }
                })?;
            Ok(ToolDefinition::with_raw_schema(
                definition.name().as_str(),
                definition.description(),
                schema,
            ))
        })
        .collect()
}

/// A correlation mismatch is a fail-closed bridge defect like any other, so
/// it is recorded through [`fail_closed`] rather than returned silently.
fn require_correlation(
    telemetry: ModelCallTelemetry,
    returned: ModelCallId,
) -> Result<(), RuntimeModelCallProviderError> {
    if telemetry.call == returned {
        Ok(())
    } else {
        Err(fail_closed(
            telemetry,
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
                ToolExecutionErrorKind::PreauthorizationRejected => "preauthorization_rejected",
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
        ModelToolResultContent::Delegation(outcome) => match (outcome.kind(), outcome.content()) {
            (DelegationOutcomeKind::ResultReturned, Some(content)) => {
                (content.as_str().to_owned(), false)
            }
            _ => (render_delegation_outcome(outcome), true),
        },
    }
}

/// Renders the compact provider-neutral JSON for a non-content delegation outcome.
pub fn render_delegation_outcome(outcome: &DelegationOutcome) -> String {
    let outcome_kind = match outcome.kind() {
        DelegationOutcomeKind::ResultReturned => "returned",
        DelegationOutcomeKind::ChildFailed => "failed",
        DelegationOutcomeKind::ChildStopped => "stopped",
        DelegationOutcomeKind::ChildCancelled => "cancelled",
        DelegationOutcomeKind::AlreadyTerminal => "already_terminal",
        DelegationOutcomeKind::ContinueRunning => "continue_running",
    };
    let reason = match outcome.reason() {
        DelegationOutcomeReason::ChildCompleted => "child_completed",
        DelegationOutcomeReason::ChildExecutionFailed => "child_execution_failed",
        DelegationOutcomeReason::ChildResultUnavailable => "child_result_unavailable",
        DelegationOutcomeReason::ChildCancelled => "child_cancelled",
        DelegationOutcomeReason::ParentStopped { .. } => "parent_stopped",
        DelegationOutcomeReason::ParentCancelled { .. } => "parent_cancelled",
    };
    let provenance = match outcome.reconstitution_provenance() {
        signalbox_domain::DelegationProvenanceReconstitutionInput::ChildTurn { session, turn } => {
            format!(
                r#"{{"type":"child_turn","child_session_id":"{}","child_turn_id":"{}"}}"#,
                session.into_uuid(),
                turn.into_uuid()
            )
        }
        signalbox_domain::DelegationProvenanceReconstitutionInput::ParentTurnCommand {
            session,
            turn,
            command,
        } => format!(
            r#"{{"type":"parent_turn_command","parent_session_id":"{}","parent_turn_id":"{}","command_id":"{}","descendant_scope":"parent_and_descendants"}}"#,
            session.into_uuid(),
            turn.into_uuid(),
            command.into_uuid()
        ),
        signalbox_domain::DelegationProvenanceReconstitutionInput::ParentGoalCommand {
            session,
            generation,
            command,
        } => format!(
            r#"{{"type":"parent_goal_command","parent_session_id":"{}","goal_generation":"{}","command_id":"{}","descendant_scope":"parent_and_descendants"}}"#,
            session.into_uuid(),
            generation.get(),
            command.into_uuid()
        ),
        signalbox_domain::DelegationProvenanceReconstitutionInput::ParentLifecycleCommand {
            session,
            command,
        } => format!(
            r#"{{"type":"parent_lifecycle_command","parent_session_id":"{}","command_id":"{}","descendant_scope":"parent_and_descendants"}}"#,
            session.into_uuid(),
            command.into_uuid()
        ),
    };
    format!(r#"{{"outcome":"{outcome_kind}","reason":"{reason}","provenance":{provenance}}}"#)
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
    diagnostic_model_identity_limit: Option<usize>,
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
                concrete_target = Some(diagnostic_model_identity(
                    reported.as_str(),
                    diagnostic_model_identity_limit,
                ));
            }
            ProviderTargetRelation::DifferentLineage => {
                return Err(ClassificationFailure {
                    error: RuntimeModelCallProviderError::ProviderTargetSubstituted,
                    served_target: Some(diagnostic_model_identity(
                        reported.as_str(),
                        diagnostic_model_identity_limit,
                    )),
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
                    AssistantPart::SuppressedToolCall(name) => {
                        tool_count += 1;
                        let Ok(name) = DomainToolName::try_new(name.as_str().to_owned()) else {
                            return classify(
                                ModelCallTerminalObservation::KnownFailed,
                                ModelCallCauseCode::UnrepresentableToolMaterial,
                            );
                        };
                        response_parts.push(AssistantResponsePart::ToolCall(
                            DomainToolCallProposal::suppressed(name),
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

fn provider_reported_token_usage(evidence: &TerminalEvidence) -> ProviderReportedTokenUsage {
    let usage = match evidence {
        TerminalEvidence::Completed(value) => value.usage,
        TerminalEvidence::Refused(value) => value.usage,
        TerminalEvidence::ProviderError(value) => value.usage,
        TerminalEvidence::BoundaryLoss(value) => value.usage,
        TerminalEvidence::CancellationConfirmed(_) | TerminalEvidence::ProvenUnsent(_) => {
            return ProviderReportedTokenUsage::unreported();
        }
    };
    ProviderReportedTokenUsage::unreported()
        .with_input_tokens(usage.input_tokens)
        .with_output_tokens(usage.output_tokens)
        .with_cache_creation_input_tokens(usage.cache_creation_input_tokens)
        .with_cache_read_input_tokens(usage.cache_read_input_tokens)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use expect_test::expect;
    use signalbox_application::{
        ClassifyOperatorFailure, ModelConversationMessage, ModelToolResultContent,
    };
    use signalbox_domain::{
        AssistantText, DelegationContent, DelegationMessageId, DelegationOutcome,
        DelegationOutcomeKind, DelegationOutcomeReason, DelegationProvenanceReconstitutionInput,
        DirectModelSelection, FastMode, FastModeOverlay, FastModeSupport, ImportedText,
        ImportedTranscriptEntryId, ModelCallId, ModelCallTerminalObservation, ModelCapabilities,
        ModelSettingsOverlay, ModelSettingsPrecedence, NormalizedToolArguments, OpenAiServiceTier,
        ProviderModelCallFailureCause, ProviderModelIdentity, ReasoningLevel,
        SemanticTranscriptEntryId, SemanticTranscriptEntryRef, ServiceTier,
        SessionConfigurationDefaultsVersion, SessionId, SettingOverlay, ToolExecutionError,
        ToolExecutionErrorKind, ToolRequest, ToolRequestId, ToolRequestOrdinal,
        ToolRequestReconstitutionInput, TurnAttemptId, TurnId,
    };
    use signalbox_expect_table::table;
    use signalbox_model_runtime::{
        AssistantPart, BoundaryLossEvidence, CancellationConfirmedEvidence, CompletionEvidence,
        CompletionFinish, ConversationMessage, CredentialAccessError, CredentialAccessFailure,
        ExchangeFacts, LossCause, NativeErrorFacts, Observation, ObservationFact, ObservationSink,
        PreparationFailure, ProvenUnsentEvidence, ProviderErrorEvidence, ProviderErrorKind,
        ProviderReportedModel, ReasoningLevel as RuntimeReasoningLevel, RefusalEvidence,
        ServiceTier as RuntimeServiceTier, TerminalEvidence, TokenUsage, ToolCallId,
        ToolCallProposal, ToolCallsAtLoss, ToolName, TransportFacts, UnsentCause,
    };
    use uuid::Uuid;

    use super::{
        AcceptanceObservations, InvalidRuntimeToolSchema, ModelCallCauseCode, ModelCallTelemetry,
        ProviderTextDelta, ProviderTextDeltaContext, ProviderTextDeltaSink,
        RuntimeInputTokenCountError, RuntimeModelCallProviderError, RuntimeModelCatalog,
        RuntimeModelCatalogError, RuntimeModelDefinition, RuntimeModelDefinitionError,
        classify_terminal as classify_terminal_with_limit, decode_checked_raw_json,
        provider_reported_token_usage, render_runtime_messages, runtime_delivery_definitions,
        runtime_model_settings,
    };
    use signalbox_domain::ResolvedProviderTarget;

    const SYNTHETIC_MALFORMED_TOOL_SCHEMA: &str = "{";
    const SYNTHETIC_INVALID_TOOL_NAME: &str = "synthetic_invalid_tool";

    fn call() -> ModelCallId {
        ModelCallId::from_uuid(Uuid::from_u128(1))
    }

    fn telemetry() -> ModelCallTelemetry {
        ModelCallTelemetry {
            session: SessionId::from_uuid(Uuid::from_u128(10)),
            turn: TurnId::from_uuid(Uuid::from_u128(11)),
            attempt: TurnAttemptId::from_uuid(Uuid::from_u128(12)),
            call: call(),
        }
    }

    /// The exact provider-model spelling one deployment configures.
    fn configured(spelling: &str) -> signalbox_model_runtime::ResolvedTarget {
        signalbox_model_runtime::ResolvedTarget::new(spelling.to_owned())
    }

    fn classify_terminal(
        evidence: TerminalEvidence,
        observations: &[Observation<ModelCallId>],
        configured_target: &signalbox_model_runtime::ResolvedTarget,
    ) -> Result<super::TerminalClassification, super::ClassificationFailure> {
        classify_terminal_with_limit(evidence, observations, configured_target, None)
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

    #[test]
    fn delegation_inputs_render_with_exact_provider_neutral_prefixes() {
        let parent = SessionId::from_uuid(Uuid::from_u128(20));
        let child = SessionId::from_uuid(Uuid::from_u128(21));
        let spawning_request = ToolRequestId::from_uuid(Uuid::from_u128(22));
        let awaiting_request = ToolRequestId::from_uuid(Uuid::from_u128(23));
        let task_content =
            DelegationContent::try_new("task bytes".into()).expect("fixture task is valid");
        let message_content =
            DelegationContent::try_new("message bytes".into()).expect("fixture message is valid");
        let result_content =
            DelegationContent::try_new("child result".into()).expect("fixture result is valid");
        let outcome = DelegationOutcome::reconstitute(
            DelegationOutcomeKind::ResultReturned,
            Some(result_content.clone()),
            DelegationOutcomeReason::ChildCompleted,
            DelegationProvenanceReconstitutionInput::ChildTurn {
                session: child,
                turn: TurnId::from_uuid(Uuid::from_u128(24)),
            },
        )
        .expect("fixture result is correlated");

        let rendered = render_runtime_messages(&[
            ModelConversationMessage::DelegatedTask {
                source: source(25),
                spawning_request,
                parent_session: parent,
                parent_turn: TurnId::from_uuid(Uuid::from_u128(26)),
                content: task_content.clone(),
            },
            ModelConversationMessage::DelegationMessage {
                source: source(27),
                spawning_request,
                message: DelegationMessageId::from_uuid(Uuid::from_u128(28)),
                sender: parent,
                recipient: child,
                delivery_sequence: std::num::NonZeroU64::MIN,
                content: message_content.clone(),
            },
            ModelConversationMessage::BackgroundDelegationResult {
                source: source(29),
                awaiting_request,
                spawning_request,
                child,
                delivery_sequence: std::num::NonZeroU64::new(2).expect("two is positive"),
                outcome,
            },
        ]);

        assert_eq!(
            rendered,
            vec![
                ConversationMessage::user_text(format!(
                    "Signalbox delegated task:\n{}",
                    task_content.as_str()
                )),
                ConversationMessage::user_text(format!(
                    "Signalbox delegation message from session {}:\n{}",
                    parent.into_uuid(),
                    message_content.as_str()
                )),
                ConversationMessage::user_text(format!(
                    "Signalbox background child result from session {}:\n{}",
                    child.into_uuid(),
                    result_content.as_str()
                )),
            ]
        );
    }

    #[test]
    fn foreground_child_failure_renders_compact_typed_tool_result() {
        let child = SessionId::from_uuid(Uuid::from_u128(30));
        let child_turn = TurnId::from_uuid(Uuid::from_u128(31));
        let request = ToolRequestId::from_uuid(Uuid::from_u128(32));
        let outcome = DelegationOutcome::reconstitute(
            DelegationOutcomeKind::ChildFailed,
            None,
            DelegationOutcomeReason::ChildExecutionFailed,
            DelegationProvenanceReconstitutionInput::ChildTurn {
                session: child,
                turn: child_turn,
            },
        )
        .expect("fixture failure is correlated");

        let rendered = render_runtime_messages(&[ModelConversationMessage::ToolResult {
            source: source(33),
            request,
            content: ModelToolResultContent::Delegation(outcome),
        }]);

        assert_eq!(
            rendered[0].role,
            signalbox_model_runtime::ConversationRole::User
        );
        assert_eq!(
            rendered[0].parts,
            vec![signalbox_model_runtime::MessagePart::ToolResult(
                signalbox_model_runtime::ToolResultRecord {
                    tool_call_id: ToolCallId::new(request.into_uuid().to_string()),
                    content: format!(
                        r#"{{"outcome":"failed","reason":"child_execution_failed","provenance":{{"type":"child_turn","child_session_id":"{}","child_turn_id":"{}"}}}}"#,
                        child.into_uuid(),
                        child_turn.into_uuid()
                    ),
                    is_error: true,
                },
            )]
        );
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

    #[test]
    fn terminal_usage_mapping_preserves_each_provider_field_exactly() {
        let reported_usage = TokenUsage {
            input_tokens: Some(120),
            output_tokens: Some(0),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(80),
        };
        let evidence = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("model-exact")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(String::from("complete"))],
            usage: reported_usage,
        });

        let usage = provider_reported_token_usage(&evidence);

        assert_eq!(usage.input_tokens(), reported_usage.input_tokens);
        assert_eq!(usage.output_tokens(), reported_usage.output_tokens);
        assert_eq!(
            usage.cache_creation_input_tokens(),
            reported_usage.cache_creation_input_tokens
        );
        assert_eq!(
            usage.cache_read_input_tokens(),
            reported_usage.cache_read_input_tokens
        );
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
            telemetry: telemetry(),
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
            telemetry: telemetry(),
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
            telemetry: ModelCallTelemetry {
                session: expected_session,
                turn: expected_turn,
                ..telemetry()
            },
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

    /// A definitive provider classification becomes the closed domain cause
    /// carried beyond the bridge, never provider-authored error prose.
    #[test]
    fn provider_failure_cause_reaches_the_domain_classification() {
        assert_eq!(
            super::provider_failure_cause(ProviderErrorKind::QuotaExhausted),
            ProviderModelCallFailureCause::QuotaExhausted
        );
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
                    non_acceptance_proven: false,
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
                    tool_calls: ToolCallsAtLoss::Unobserved,
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

    /// A CLI-redacted argument object becomes an inert domain proposal so the
    /// application can record its runtime-safety denial and continue the turn.
    #[test]
    fn fully_suppressed_tool_arguments_cross_as_inert_proposal() {
        let classified = classify_terminal(
            completion_with_finish(
                "model-exact",
                CompletionFinish::ToolUse,
                vec![AssistantPart::SuppressedToolCall(ToolName::new(
                    "current_time",
                ))],
            ),
            &[],
            &configured("model-exact"),
        )
        .expect("suppressed tool material has a bounded terminal classification");

        let ModelCallTerminalObservation::CompletedWithTools { response } = classified.observation
        else {
            panic!("suppressed tool material yields a same-turn denial round");
        };
        assert_eq!(classified.cause, ModelCallCauseCode::Completed);
        let signalbox_domain::AssistantResponsePart::ToolCall(proposal) = &response.parts()[0]
        else {
            panic!("the response retains the inert tool proposal");
        };
        assert_eq!(proposal.name().as_str(), "current_time");
        assert_eq!(
            proposal.arguments().as_str(),
            r#"{"redacted":"[redacted]"}"#
        );
        assert!(proposal.is_suppressed());
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
                    non_acceptance_proven: false,
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
                    tool_calls: ToolCallsAtLoss::Unobserved,
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
            super::diagnostic_model_identity(&reported, None).len(),
            reported.len()
        );

        let configured_limit = 17;
        let hostile = "x".repeat(configured_limit * 4);
        let bounded = super::diagnostic_model_identity(&hostile, Some(configured_limit));
        assert!(bounded.starts_with(&"x".repeat(configured_limit)));
        assert!(bounded.ends_with("… [truncated]"));
    }

    #[test]
    fn input_token_count_failures_keep_exact_operator_causes() {
        assert_eq!(
            RuntimeInputTokenCountError::UnconfiguredTarget.operator_failure_cause_code(),
            "model_input_count_unconfigured_target"
        );
        assert_eq!(
            RuntimeInputTokenCountError::InvalidToolSchema.operator_failure_cause_code(),
            "model_input_count_invalid_tool_schema"
        );
        assert_eq!(
            RuntimeInputTokenCountError::CorrelationMismatch.operator_failure_cause_code(),
            "model_input_count_correlation_mismatch"
        );
    }

    #[test]
    fn failed_input_estimate_falls_through_to_durable_call_path() {
        assert_eq!(
            super::classify_runtime_input_count(
                signalbox_model_runtime::InputTokenCountOutcome::Failed {
                    correlation: call(),
                },
                call(),
            ),
            Ok(signalbox_application::ModelCallInputTokenCount::Unavailable)
        );
    }

    #[test]
    fn invalid_runtime_tool_schema_retains_its_json_source() {
        let source = serde_json::from_str::<serde_json::Value>(SYNTHETIC_MALFORMED_TOOL_SCHEMA)
            .expect_err("synthetic schema is invalid JSON");
        let expected_source = source.to_string();
        let error = InvalidRuntimeToolSchema {
            tool_name: String::from(SYNTHETIC_INVALID_TOOL_NAME),
            source,
        };

        expect![[
            "application tool schema is invalid at the runtime bridge: synthetic_invalid_tool"
        ]]
        .assert_eq(&error.to_string());
        assert_eq!(error.tool_name(), SYNTHETIC_INVALID_TOOL_NAME);
        assert_eq!(
            std::error::Error::source(&error).map(ToString::to_string),
            Some(expected_source)
        );
    }

    #[test]
    fn output_reservation_cannot_exceed_context_window() {
        assert_eq!(
            RuntimeModelDefinition::try_new(target(1), String::from("fixture-model"), 65, 64,),
            Err(RuntimeModelDefinitionError::OutputLimitExceedsContextWindow)
        );
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

    #[test]
    fn validated_domain_settings_map_completely_into_runtime_settings() {
        let selection = DirectModelSelection::from_uuid(Uuid::from_u128(1));
        let capabilities = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::Max]),
            FastModeSupport::RequestControl,
            BTreeSet::from([ServiceTier::OpenAi(OpenAiServiceTier::Priority)]),
        );
        let precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::Max),
                FastModeOverlay::Value(FastMode::Enabled),
                SettingOverlay::Value(ServiceTier::OpenAi(OpenAiServiceTier::Priority)),
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let validated = capabilities
            .validate_precedence(selection, precedence)
            .expect("fixture settings are declared by the capability record");

        let mapped = runtime_model_settings(512, validated);

        assert_eq!(mapped.max_output_tokens, 512);
        assert_eq!(mapped.reasoning_level, Some(RuntimeReasoningLevel::Max));
        assert_eq!(mapped.fast_mode, signalbox_model_runtime::FastMode::Enabled);
        assert_eq!(
            mapped.service_tier,
            Some(RuntimeServiceTier::OpenAi(
                signalbox_model_runtime::OpenAiServiceTier::Priority
            ))
        );
    }

    #[test]
    fn mapped_fast_target_preserves_the_toggle_for_adapter_mapping() {
        let selection = DirectModelSelection::from_uuid(Uuid::from_u128(1));
        let capabilities = ModelCapabilities::new(
            BTreeSet::new(),
            FastModeSupport::AlternateTarget(target(2)),
            BTreeSet::new(),
        );
        let precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::new(
                SettingOverlay::Inherit,
                FastModeOverlay::Value(FastMode::Enabled),
                SettingOverlay::Inherit,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let validated = capabilities
            .validate_precedence(selection, precedence)
            .expect("mapped fast serving is declared by the capability record");

        let mapped = runtime_model_settings(512, validated);

        assert_eq!(mapped.fast_mode, signalbox_model_runtime::FastMode::Enabled);
    }

    #[test]
    fn mapped_fast_target_supplies_the_authorized_delivery_identity_and_limit() {
        let selected_model = "fixture-standard";
        let serving_model = "fixture-fast";
        let selected_output_limit = 64;
        let serving_output_limit = 32;
        let ordinary = RuntimeModelDefinition::try_new(
            target(1),
            String::from(selected_model),
            selected_output_limit,
            200_000,
        )
        .expect("ordinary fixture definition is valid")
        .with_fast_target(target(2));
        let fast = RuntimeModelDefinition::try_new(
            target(2),
            String::from(serving_model),
            serving_output_limit,
            200_000,
        )
        .expect("fast fixture definition is valid");
        let catalog = RuntimeModelCatalog::try_from_definitions([ordinary, fast])
            .expect("mapped target is present");
        let source = catalog
            .resolve(target(1))
            .expect("source target is present");
        let (selected, serving) =
            runtime_delivery_definitions(&catalog, target(1), FastMode::Enabled)
                .expect("mapped delivery resolves");

        assert_eq!(selected.provider_model(), selected_model);
        assert_eq!(serving.provider_model(), serving_model);

        assert_eq!(
            catalog
                .effective_definition(source, FastMode::Disabled)
                .expect("ordinary target resolves")
                .provider_model(),
            selected_model
        );
        assert_eq!(
            catalog
                .effective_definition(source, FastMode::Disabled)
                .expect("ordinary target resolves")
                .max_output_tokens(),
            selected_output_limit
        );
        assert_eq!(
            catalog
                .effective_definition(source, FastMode::Enabled)
                .expect("mapped fast target resolves")
                .provider_model(),
            serving_model
        );
        assert_eq!(
            catalog
                .effective_definition(source, FastMode::Enabled)
                .expect("mapped fast target resolves")
                .max_output_tokens(),
            serving_output_limit
        );
    }

    #[test]
    fn runtime_catalog_rejects_a_missing_mapped_fast_target() {
        let ordinary = RuntimeModelDefinition::try_new(
            target(1),
            String::from("fixture-standard"),
            64,
            200_000,
        )
        .expect("ordinary fixture definition is valid")
        .with_fast_target(target(2));

        assert_eq!(
            RuntimeModelCatalog::try_from_definitions([ordinary]),
            Err(RuntimeModelCatalogError::MissingFastTarget {
                target: target(1),
                fast_target: target(2),
            })
        );
    }
}
