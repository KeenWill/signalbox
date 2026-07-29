//! Per-operation sampling and limit settings.

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
        }
    }
}
