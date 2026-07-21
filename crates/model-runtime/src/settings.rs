//! Per-operation sampling and limit settings.

/// Sampling and output-limit settings for one operation.
///
/// The output-token ceiling is required because the smoke-critical provider
/// contract requires it on every request; optional knobs are sent only when
/// set, so provider defaults apply otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSettings {
    /// Hard ceiling on generated output tokens.
    pub max_output_tokens: u32,
    /// Sampling temperature, when the caller sets one.
    pub temperature: Option<f64>,
    /// Nucleus-sampling probability mass, when the caller sets one.
    pub top_p: Option<f64>,
    /// Sequences at which the provider must stop generating.
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
