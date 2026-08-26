//! Resource bounds for provider-controlled JSON.

use std::cell::Cell;
use std::collections::HashSet;
use std::fmt;

use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

/// Maximum permitted nesting of JSON object and array containers in one
/// provider-controlled value.
// numeric-bound: guard - prevents pathological provider-JSON nesting from exhausting the stack
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

struct DuplicateFreeJson<'a> {
    duplicate_found: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for DuplicateFreeJson<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateFreeVisitor {
            duplicate_found: self.duplicate_found,
        })
    }
}

struct DuplicateFreeVisitor<'a> {
    duplicate_found: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for DuplicateFreeVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without repeated object members")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        DuplicateFreeJson {
            duplicate_found: self.duplicate_found,
        }
        .deserialize(deserializer)
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        DuplicateFreeJson {
            duplicate_found: self.duplicate_found,
        }
        .deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence
            .next_element_seed(DuplicateFreeJson {
                duplicate_found: self.duplicate_found,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut members = HashSet::new();
        while let Some(member) = object.next_key::<String>()? {
            if !members.insert(member) {
                self.duplicate_found.set(true);
                return Err(serde::de::Error::custom("duplicate JSON member"));
            }
            object.next_value_seed(DuplicateFreeJson {
                duplicate_found: self.duplicate_found,
            })?;
        }
        Ok(())
    }
}

/// Reports whether a syntactically valid provider JSON value repeats an object
/// member at any nesting depth.
///
/// Values beyond [`PROVIDER_JSON_NESTING_LIMIT`] are rejected explicitly before
/// serde can stop at its own recursion limit. Malformed input within the bound
/// remains the typed decoder's responsibility and carries no duplicate-detection
/// guarantee: parsing stops at the first syntax error, so a repeat after that
/// point is not observed. This scan exists only to detect the ambiguity that
/// serde's last-value-wins object projection would otherwise erase from valid
/// input.
pub fn provider_json_has_duplicate_members(
    text: &str,
) -> Result<bool, ProviderJsonNestingExceeded> {
    validate_provider_json_nesting(text.as_bytes())?;
    let duplicate_found = Cell::new(false);
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let _ = DuplicateFreeJson {
        duplicate_found: &duplicate_found,
    }
    .deserialize(&mut deserializer)
    .and_then(|()| deserializer.end());
    Ok(duplicate_found.get())
}

#[cfg(test)]
mod tests {
    use super::{
        PROVIDER_JSON_NESTING_LIMIT, ProviderJsonNestingExceeded, ProviderJsonNestingValidator,
        provider_json_has_duplicate_members, validate_provider_json_nesting,
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

    #[test]
    fn duplicate_member_scan_checks_every_object_depth() {
        assert_eq!(
            provider_json_has_duplicate_members(r#"{"event":"first","event":"second"}"#),
            Ok(true)
        );
        assert_eq!(
            provider_json_has_duplicate_members(r#"{"outer":{"event":"first","event":"second"}}"#),
            Ok(true)
        );
        assert_eq!(
            provider_json_has_duplicate_members(r#"{"event":"first","nested":{"event":"second"}}"#),
            Ok(false)
        );
        assert_eq!(provider_json_has_duplicate_members("{"), Ok(false));
    }

    #[test]
    fn duplicate_member_scan_continues_after_an_out_of_range_number() {
        const PROVIDER_JSON: &str = r#"{"number":1e1000000,"event":"first","event":"second"}"#;

        assert_eq!(provider_json_has_duplicate_members(PROVIDER_JSON), Ok(true));
    }

    #[test]
    fn duplicate_member_scan_reports_the_shared_nesting_limit() {
        let depth = PROVIDER_JSON_NESTING_LIMIT + 1;
        let json = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));

        assert_eq!(
            provider_json_has_duplicate_members(&json),
            Err(ProviderJsonNestingExceeded)
        );
    }

    #[test]
    fn malformed_input_before_a_repeat_is_left_to_the_typed_decoder() {
        const MALFORMED_PROVIDER_JSON: &str = r#"{"bad": tru, "dup": 1, "dup": 2}"#;

        assert_eq!(
            provider_json_has_duplicate_members(MALFORMED_PROVIDER_JSON),
            Ok(false)
        );
    }
}
