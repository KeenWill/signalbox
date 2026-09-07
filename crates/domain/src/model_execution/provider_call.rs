//! Model-call provider call for `docs/spec/model-call-execution.md`.

use crate::{
    AssistantResponsePart, AssistantText, ContextFrontierId, ModelCallDisposition, ModelCallId,
    ProviderCompactionBlock, ResolvedProviderTarget, SessionId, ToolUsingAssistantResponse,
    TurnAttemptId, TurnId,
};
use std::time::Duration;

/// Sealed issued-call facts carried across one provider interaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IssuedModelCallCorrelation {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) attempt: TurnAttemptId,
    pub(super) call: ModelCallId,
    pub(super) target: ResolvedProviderTarget,
    pub(super) frontier: ContextFrontierId,
}

impl IssuedModelCallCorrelation {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the exact issued physical attempt.
    pub const fn attempt(&self) -> TurnAttemptId {
        self.attempt
    }

    /// Returns the exact issued model call.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the exact pinned target used by the issued call.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }

    /// Returns the exact context frontier used by the issued call.
    pub const fn frontier(&self) -> ContextFrontierId {
        self.frontier
    }

    /// Binds one provider-neutral terminal observation to these issued facts.
    pub fn bind_terminal_observation(
        self,
        observation: ModelCallTerminalObservation,
    ) -> CorrelatedModelCallTerminalObservation {
        self.bind_terminal_observation_with_usage(
            observation,
            ProviderReportedTokenUsage::unreported(),
        )
    }

    /// Binds one provider-neutral terminal observation and the exact
    /// provider-reported token fields to these issued facts.
    pub fn bind_terminal_observation_with_usage(
        self,
        observation: ModelCallTerminalObservation,
        usage: ProviderReportedTokenUsage,
    ) -> CorrelatedModelCallTerminalObservation {
        CorrelatedModelCallTerminalObservation {
            correlation: self,
            observation,
            usage,
            provider_failure_cause: None,
            retry_after: None,
            non_acceptance_proven: false,
            rate_limits: None,
        }
    }

    /// Binds one definitive, classified provider error without retaining any
    /// provider-authored error material.
    pub fn bind_provider_failure_observation_with_usage(
        self,
        cause: ProviderModelCallFailureCause,
        usage: ProviderReportedTokenUsage,
    ) -> CorrelatedModelCallTerminalObservation {
        self.bind_provider_failure_observation_with_retry_after(cause, usage, None, false)
    }

    /// Binds a classified provider error and its optional provider-directed
    /// retry delay without retaining provider-authored error material.
    pub fn bind_provider_failure_observation_with_retry_after(
        self,
        cause: ProviderModelCallFailureCause,
        usage: ProviderReportedTokenUsage,
        retry_after: Option<Duration>,
        non_acceptance_proven: bool,
    ) -> CorrelatedModelCallTerminalObservation {
        CorrelatedModelCallTerminalObservation {
            correlation: self,
            observation: ModelCallTerminalObservation::KnownFailed,
            usage,
            provider_failure_cause: Some(cause),
            retry_after,
            non_acceptance_proven,
            rate_limits: None,
        }
    }
}

/// Token usage exactly as reported for one provider interaction.
///
/// Each field is independently absent when the provider did not report it.
/// In particular, a reported zero remains distinct from absence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProviderReportedTokenUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

impl ProviderReportedTokenUsage {
    /// Returns usage with every field unreported.
    pub const fn unreported() -> Self {
        Self {
            input_tokens: None,
            output_tokens: None,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }
    }

    /// Retains the provider's input-token field exactly.
    pub const fn with_input_tokens(mut self, input_tokens: Option<u64>) -> Self {
        self.input_tokens = input_tokens;
        self
    }

    /// Retains the provider's output-token field exactly.
    pub const fn with_output_tokens(mut self, output_tokens: Option<u64>) -> Self {
        self.output_tokens = output_tokens;
        self
    }

    /// Retains the provider's cache-creation input-token field exactly.
    pub const fn with_cache_creation_input_tokens(
        mut self,
        cache_creation_input_tokens: Option<u64>,
    ) -> Self {
        self.cache_creation_input_tokens = cache_creation_input_tokens;
        self
    }

    /// Retains the provider's cache-read input-token field exactly.
    pub const fn with_cache_read_input_tokens(
        mut self,
        cache_read_input_tokens: Option<u64>,
    ) -> Self {
        self.cache_read_input_tokens = cache_read_input_tokens;
        self
    }

    /// Returns the provider's input-token field.
    pub const fn input_tokens(self) -> Option<u64> {
        self.input_tokens
    }

    /// Returns the provider's output-token field.
    pub const fn output_tokens(self) -> Option<u64> {
        self.output_tokens
    }

    /// Returns the provider's cache-creation input-token field.
    pub const fn cache_creation_input_tokens(self) -> Option<u64> {
        self.cache_creation_input_tokens
    }

    /// Returns the provider's cache-read input-token field.
    pub const fn cache_read_input_tokens(self) -> Option<u64> {
        self.cache_read_input_tokens
    }
}

/// Closed, provider-neutral classification of one definitive provider error.
///
/// These values contain no provider-authored text, credential material, model
/// content, or request/response body. Absence on a known failure means the
/// failure arose outside a definitive provider error or predates persistence of
/// this classification.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProviderModelCallFailureCause {
    /// The provider rejected the request credential.
    CredentialRejected,
    /// The credential was valid but lacked permission.
    PermissionDenied,
    /// The provider judged the request invalid.
    InvalidRequest,
    /// The requested model or resource was not found.
    TargetNotFound,
    /// The request exceeded a provider size limit.
    RequestTooLarge,
    /// The provider applied a transient rate limit.
    RateLimited,
    /// The account's available quota was exhausted.
    QuotaExhausted,
    /// The provider reported overload.
    Overloaded,
    /// The provider reported an internal error.
    ProviderInternal,
    /// The adapter did not recognize a definitive provider error class.
    Unrecognized,
}

/// One provider-neutral terminal observation bound to exact issued authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorrelatedModelCallTerminalObservation {
    pub(super) rate_limits: Option<Box<crate::ProviderRateLimitSnapshot>>,
    pub(super) correlation: IssuedModelCallCorrelation,
    pub(super) observation: ModelCallTerminalObservation,
    pub(super) usage: ProviderReportedTokenUsage,
    pub(super) provider_failure_cause: Option<ProviderModelCallFailureCause>,
    pub(super) retry_after: Option<Duration>,
    pub(super) non_acceptance_proven: bool,
}

impl CorrelatedModelCallTerminalObservation {
    /// Attaches capacity evidence observed during this exact provider call.
    pub fn with_rate_limits(mut self, snapshot: Option<crate::ProviderRateLimitSnapshot>) -> Self {
        self.rate_limits = snapshot.map(Box::new);
        self
    }

    /// Borrows the latest reported capacity snapshot, if any.
    pub fn rate_limits(&self) -> Option<&crate::ProviderRateLimitSnapshot> {
        self.rate_limits.as_deref()
    }

    /// Returns the exact model call named by the issued correlation.
    pub const fn call(&self) -> ModelCallId {
        self.correlation.call
    }

    /// Borrows all exact issued facts carried with the observation.
    pub const fn correlation(&self) -> &IssuedModelCallCorrelation {
        &self.correlation
    }

    /// Borrows the provider-neutral physical outcome.
    pub const fn observation(&self) -> &ModelCallTerminalObservation {
        &self.observation
    }

    /// Returns the exact provider-reported token fields.
    pub const fn usage(&self) -> ProviderReportedTokenUsage {
        self.usage
    }

    /// Returns the closed provider classification when a definitive provider
    /// error caused this known failure.
    pub const fn provider_failure_cause(&self) -> Option<ProviderModelCallFailureCause> {
        self.provider_failure_cause
    }

    /// Returns the provider-directed minimum delay before another
    /// availability attempt, when the provider supplied one.
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Reports whether adapter protocol evidence authorizes substitution.
    pub const fn non_acceptance_proven(&self) -> bool {
        self.non_acceptance_proven
    }
}

/// One exact scripted or provider-adapter terminal classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalObservation {
    /// Definitive success with the complete ordered text-only response.
    Completed {
        /// Exact assistant text parts in final semantic order.
        assistant_text: Vec<AssistantText>,
    },
    /// Definitive success containing provider compaction blocks and no tools.
    CompletedWithProviderCompaction {
        /// Exact text and provider compaction parts in provider order.
        response: Vec<AssistantResponsePart>,
        /// Provider-reported input retained after its final compaction
        /// iteration, including cache axes and excluding earlier billed
        /// iterations.
        retained_input_tokens: u64,
        /// Provider-reported output from the final physical iteration.
        retained_output_tokens: u64,
    },
    /// Definitive success whose ordered response contains tool proposals.
    CompletedWithTools {
        /// Ordered text and normalized proposals, proven to contain a tool.
        response: ToolUsingAssistantResponse,
        /// Provider-reported input retained after an in-response compaction,
        /// when the tool response contains a provider compaction block.
        retained_input_tokens: Option<u64>,
        /// Provider-reported final-iteration output paired with retained input.
        retained_output_tokens: Option<u64>,
    },
    /// Evidence establishes a known failure.
    KnownFailed,
    /// The authenticated complete exchange was explicitly refused.
    Refused,
    /// A refused exchange that first produced durable provider compaction.
    RefusedWithProviderCompaction {
        /// Provider compaction blocks in response order; ordinary refusal text
        /// remains non-transcript evidence.
        provider_compaction: Vec<ProviderCompactionBlock>,
        /// Provider-reported input retained after the final compaction iteration.
        retained_input_tokens: u64,
        /// Provider-reported output from the final physical iteration.
        retained_output_tokens: u64,
    },
    /// The physical provider interaction definitively cancelled.
    Cancelled,
    /// Provider acceptance or completion remains unresolved.
    Ambiguous,
}

impl ModelCallTerminalObservation {
    /// Returns provider-reported retained input for a completed in-response
    /// compaction, separate from billed physical-iteration usage.
    pub const fn retained_input_tokens(&self) -> Option<u64> {
        match self {
            Self::CompletedWithProviderCompaction {
                retained_input_tokens,
                ..
            } => Some(*retained_input_tokens),
            Self::CompletedWithTools {
                retained_input_tokens,
                ..
            } => *retained_input_tokens,
            Self::RefusedWithProviderCompaction {
                retained_input_tokens,
                ..
            } => Some(*retained_input_tokens),
            _ => None,
        }
    }

    /// Returns provider-reported final-iteration output for an in-response
    /// compaction, separate from billed physical-iteration usage.
    pub const fn retained_output_tokens(&self) -> Option<u64> {
        match self {
            Self::CompletedWithProviderCompaction {
                retained_output_tokens,
                ..
            } => Some(*retained_output_tokens),
            Self::CompletedWithTools {
                retained_output_tokens,
                ..
            } => *retained_output_tokens,
            Self::RefusedWithProviderCompaction {
                retained_output_tokens,
                ..
            } => Some(*retained_output_tokens),
            _ => None,
        }
    }

    /// Returns the exact physical disposition declared by this observation.
    pub const fn disposition(&self) -> ModelCallDisposition {
        match self {
            Self::Completed { .. }
            | Self::CompletedWithProviderCompaction { .. }
            | Self::CompletedWithTools { .. } => ModelCallDisposition::Completed,
            Self::KnownFailed => ModelCallDisposition::KnownFailed,
            Self::Refused | Self::RefusedWithProviderCompaction { .. } => {
                ModelCallDisposition::Refused
            }
            Self::Cancelled => ModelCallDisposition::Cancelled,
            Self::Ambiguous => ModelCallDisposition::Ambiguous,
        }
    }

    pub(super) const fn is_tool_round(&self) -> bool {
        matches!(self, Self::CompletedWithTools { .. })
    }
}
