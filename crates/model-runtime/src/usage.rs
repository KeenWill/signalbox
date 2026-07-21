//! Provider-reported token usage.

/// Token usage as reported by the provider, absent where unreported.
///
/// Usage is evidence for the caller's budget accounting; this layer only
/// records what the provider stated and never estimates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// Input tokens billed for the request.
    pub input_tokens: Option<u64>,
    /// Output tokens generated.
    pub output_tokens: Option<u64>,
    /// Input tokens written to the provider's prompt cache.
    pub cache_creation_input_tokens: Option<u64>,
    /// Input tokens read from the provider's prompt cache.
    pub cache_read_input_tokens: Option<u64>,
}

impl TokenUsage {
    /// Usage with every field unreported.
    pub fn unreported() -> Self {
        Self::default()
    }

    /// Folds a later report into this one: a later reported field replaces
    /// the earlier value, and an unreported field never erases one.
    ///
    /// Streaming providers report usage incrementally (input-side counts at
    /// stream start, output counts with terminal metadata); absorbing each
    /// report yields the final observed usage.
    pub fn absorb(&mut self, later: TokenUsage) {
        if later.input_tokens.is_some() {
            self.input_tokens = later.input_tokens;
        }
        if later.output_tokens.is_some() {
            self.output_tokens = later.output_tokens;
        }
        if later.cache_creation_input_tokens.is_some() {
            self.cache_creation_input_tokens = later.cache_creation_input_tokens;
        }
        if later.cache_read_input_tokens.is_some() {
            self.cache_read_input_tokens = later.cache_read_input_tokens;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TokenUsage;

    #[test]
    fn absorb_replaces_reported_fields_and_keeps_unreported_ones() {
        let mut usage = TokenUsage {
            input_tokens: Some(120),
            output_tokens: Some(1),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(80),
        };

        usage.absorb(TokenUsage {
            input_tokens: None,
            output_tokens: Some(47),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        });

        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: Some(120),
                output_tokens: Some(47),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(80),
            }
        );
    }
}
