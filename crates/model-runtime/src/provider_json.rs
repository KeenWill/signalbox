//! Resource bounds for provider-controlled JSON.

/// Maximum permitted nesting of JSON object and array containers in one
/// provider-controlled value.
pub const PROVIDER_JSON_NESTING_LIMIT: usize = 128;

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

/// Checks the object/array nesting of provider-controlled JSON bytes.
///
/// The scan does not allocate. Braces and brackets inside JSON strings,
/// including after escaped quotes and backslashes, do not affect the depth.
/// JSON syntax remains the typed decoder's responsibility.
pub fn validate_provider_json_nesting(bytes: &[u8]) -> Result<(), ProviderJsonNestingExceeded> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else {
                match byte {
                    b'\\' => escaped = true,
                    b'"' => in_string = false,
                    _ => {}
                }
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > PROVIDER_JSON_NESTING_LIMIT {
                    return Err(ProviderJsonNestingExceeded);
                }
            }
            b'}' | b']' if depth > 0 => depth -= 1,
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PROVIDER_JSON_NESTING_LIMIT, validate_provider_json_nesting};

    #[test]
    fn accepts_the_exact_provider_json_nesting_limit() {
        let json = format!(
            "{}0{}",
            "[".repeat(PROVIDER_JSON_NESTING_LIMIT),
            "]".repeat(PROVIDER_JSON_NESTING_LIMIT)
        );

        assert_eq!(validate_provider_json_nesting(json.as_bytes()), Ok(()));
    }

    #[test]
    fn rejects_one_container_beyond_the_provider_json_nesting_limit() {
        let depth = PROVIDER_JSON_NESTING_LIMIT + 1;
        let json = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));

        assert!(validate_provider_json_nesting(json.as_bytes()).is_err());
    }

    #[test]
    fn ignores_container_tokens_inside_strings_and_escaped_quotes() {
        let json = br#"{"text":"[ { before an escaped quote: \" } ] after it","value":[]}"#;

        assert_eq!(validate_provider_json_nesting(json), Ok(()));
    }
}
