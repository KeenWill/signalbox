//! Per-operation generation, reasoning, and serving settings.

/// Provider-neutral reasoning effort in ascending spend/depth order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasoningLevel {
    /// Explicitly request no reasoning effort.
    None,
    /// The smallest provider effort above none.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort.
    XHigh,
    /// Maximum reasoning effort.
    Max,
    /// Codex Ultra reasoning effort.
    Ultra,
}

/// Whether provider fast mode is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FastMode {
    /// Ordinary serving speed.
    Disabled,
    /// Provider fast serving mode.
    Enabled,
}

/// Anthropic service-tier values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AnthropicServiceTier {
    /// Use Priority capacity when available.
    Auto,
    /// Use only standard capacity.
    StandardOnly,
}

/// OpenAI service-tier values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpenAiServiceTier {
    /// Project-configured automatic tier.
    Auto,
    /// Default processing.
    Default,
    /// Flex processing.
    Flex,
    /// Scale processing.
    Scale,
    /// Priority processing.
    Priority,
    /// Fast processing.
    Fast,
}

/// Codex CLI service-tier values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CodexCliServiceTier {
    /// Default processing.
    Default,
    /// Priority processing.
    Priority,
    /// Flex processing.
    Flex,
}

/// Provider-tagged service-tier value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServiceTier {
    /// Anthropic Messages service tier.
    Anthropic(AnthropicServiceTier),
    /// OpenAI service tier.
    OpenAi(OpenAiServiceTier),
    /// Codex CLI service tier.
    CodexCli(CodexCliServiceTier),
}

/// Sampling and output-limit settings for one operation.
///
/// The output-token ceiling is required because the smoke-critical provider
/// contract requires it on every request. Adapters enforce these settings
/// through provider controls unless their owning specification records a
/// capability-limited advisory exception; optional knobs are omitted when
/// unset so provider defaults apply.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSettings {
    /// Requested ceiling on generated output tokens; hard unless the owning
    /// adapter specification records an advisory exception.
    pub max_output_tokens: u32,
    /// Sampling temperature, when the caller sets one.
    pub temperature: Option<f64>,
    /// Nucleus-sampling probability mass, when the caller sets one.
    pub top_p: Option<f64>,
    /// Sequences at which the provider is requested to stop generating.
    pub stop_sequences: Vec<String>,
    /// Explicit reasoning effort, or provider default when absent.
    pub reasoning_level: Option<ReasoningLevel>,
    /// Request/session fast-mode selection.
    pub fast_mode: FastMode,
    /// Explicit provider-tagged service tier, or provider default when absent.
    pub service_tier: Option<ServiceTier>,
}

impl ModelSettings {
    /// Settings carrying only the required output-token ceiling; every
    /// optional knob stays unset.
    pub fn new(max_output_tokens: u32) -> Self {
        Self {
            max_output_tokens,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            reasoning_level: None,
            fast_mode: FastMode::Disabled,
            service_tier: None,
        }
    }
}
