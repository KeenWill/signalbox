//! Resource bounds for provider-controlled JSON.

/// Maximum permitted nesting of JSON object and array containers in one
/// provider-controlled value.
pub const PROVIDER_JSON_NESTING_LIMIT: usize = 127;

/// Provider-controlled JSON exceeds [`PROVIDER_JSON_NESTING_LIMIT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderJsonNestingExceeded;

impl std::fmt::Display for ProviderJsonNestingExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider JSON exceeds the {PROVIDER_JSON_NESTING_LIMIT}-container nesting limit"
        )
    }
}

impl std::error::Error for ProviderJsonNestingExceeded {}

/// Incrementally checks one provider-controlled JSON value across fragments.
///
/// String, escape, and container-depth state is retained between calls, so a
/// caller can reject excessive nesting before forwarding or retaining each
/// fragment without rescanning the accumulated value.
#[derive(Debug, Default)]
pub struct ProviderJsonNestingValidator {
    depth: usize,
    in_string: bool,
    escaped: bool,
    exceeded: bool,
}

impl ProviderJsonNestingValidator {
    /// Starts validation at the beginning of one JSON value.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks the next contiguous fragment of the value.
    ///
    /// JSON syntax remains the typed decoder's responsibility; this method
    /// enforces only the container-nesting bound.
    pub fn validate_fragment(&mut self, bytes: &[u8]) -> Result<(), ProviderJsonNestingExceeded> {
        if self.exceeded {
            return Err(ProviderJsonNestingExceeded);
        }
        for &byte in bytes {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else {
                    match byte {
                        b'\\' => self.escaped = true,
                        b'"' => self.in_string = false,
                        _ => {}
                    }
                }
                continue;
            }

            match byte {
                b'"' => self.in_string = true,
                b'{' | b'[' => {
                    self.depth += 1;
                    if self.depth > PROVIDER_JSON_NESTING_LIMIT {
                        self.exceeded = true;
                        return Err(ProviderJsonNestingExceeded);
                    }
                }
                b'}' | b']' if self.depth > 0 => self.depth -= 1,
                _ => {}
            }
        }

        Ok(())
    }
}

/// Checks the object/array nesting of provider-controlled JSON bytes.
///
/// The scan does not allocate. Braces and brackets inside JSON strings,
/// including after escaped quotes and backslashes, do not affect the depth.
/// JSON syntax remains the typed decoder's responsibility.
pub fn validate_provider_json_nesting(bytes: &[u8]) -> Result<(), ProviderJsonNestingExceeded> {
    ProviderJsonNestingValidator::new().validate_fragment(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        PROVIDER_JSON_NESTING_LIMIT, ProviderJsonNestingValidator, validate_provider_json_nesting,
    };

    #[test]
    fn accepts_the_exact_provider_json_nesting_limit() {
        let json = format!(
            "{}0{}",
            "[".repeat(PROVIDER_JSON_NESTING_LIMIT),
            "]".repeat(PROVIDER_JSON_NESTING_LIMIT)
        );

        assert_eq!(validate_provider_json_nesting(json.as_bytes()), Ok(()));
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
    }

    #[test]
    fn rejects_one_container_beyond_the_provider_json_nesting_limit() {
        let depth = PROVIDER_JSON_NESTING_LIMIT + 1;
        let json = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));

        assert!(validate_provider_json_nesting(json.as_bytes()).is_err());

        let mut validator = ProviderJsonNestingValidator::new();
        assert_eq!(
            validator.validate_fragment("[".repeat(PROVIDER_JSON_NESTING_LIMIT).as_bytes()),
            Ok(())
        );
        assert!(validator.validate_fragment(b"[").is_err());
        assert!(validator.validate_fragment(b"]").is_err());
    }

    #[test]
    fn ignores_container_tokens_inside_strings_and_escaped_quotes() {
        let json = br#"{"text":"[ { before an escaped quote: \" } ] after it","value":[]}"#;

        assert_eq!(validate_provider_json_nesting(json), Ok(()));

        let mut validator = ProviderJsonNestingValidator::new();
        assert_eq!(
            validator.validate_fragment(br#"{"text":"escaped quote: \"#),
            Ok(())
        );
        assert_eq!(
            validator.validate_fragment(br#""[still string]","value":["#),
            Ok(())
        );
        assert_eq!(validator.validate_fragment(b"]}"), Ok(()));
    }
}
