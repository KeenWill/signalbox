//! Lossless JSON parsing shared by source-format edge converters.
//!
//! The parser preserves object member order, duplicate member names, and exact
//! JSON number spellings in Signalbox's source-neutral structured-value
//! algebra.

use std::{error::Error, fmt, str};

use serde::{
    Deserialize as _,
    de::{DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::value::RawValue;

use signalbox_domain::{
    ImportedJsonNumber, ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
};

const MAX_CONTAINER_DEPTH: usize = 128;

/// Content-silent reason one JSON record could not be normalized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonFailure {
    /// The record is not valid UTF-8.
    InvalidUtf8,
    /// The record is not valid JSON.
    Syntax,
    /// The record exceeds the maximum structured-value depth.
    DepthExceeded,
}

impl fmt::Display for JsonFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => formatter.write_str("JSON record is not valid UTF-8"),
            Self::Syntax => formatter.write_str("JSON record has invalid syntax"),
            Self::DepthExceeded => formatter.write_str("JSON record exceeds the nesting limit"),
        }
    }
}

impl Error for JsonFailure {}

/// Content-silent reason physical JSONL records could not be enumerated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonlRecordSplitFailure {
    /// A one-based physical line number could not be represented.
    PositionExhausted,
}

impl fmt::Display for JsonlRecordSplitFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSONL record splitting failed")
    }
}

impl Error for JsonlRecordSplitFailure {}

/// One physical JSONL record and its one-based source line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonlRecord<'source> {
    line: u64,
    bytes: &'source [u8],
}

impl<'source> JsonlRecord<'source> {
    /// Returns the one-based physical source line.
    pub const fn line(self) -> u64 {
        self.line
    }

    /// Borrows the exact record bytes without its line delimiter.
    pub const fn bytes(self) -> &'source [u8] {
        self.bytes
    }
}

/// Converts a zero-based collection index to its checked one-based ordinal.
pub fn one_based_ordinal(index: usize) -> Option<u64> {
    u64::try_from(index)
        .ok()
        .and_then(|value| value.checked_add(1))
}

/// Enumerates physical JSONL records without interpreting their contents.
///
/// LF terminates a record and is excluded. One immediately preceding CR is
/// also excluded. A trailing delimiter creates no extra record, while every
/// other empty physical record remains present so a caller can report it
/// without losing later records.
pub fn split_jsonl_records(source: &[u8]) -> Result<Vec<JsonlRecord<'_>>, JsonlRecordSplitFailure> {
    let mut records = Vec::new();
    let mut start = 0_usize;
    let mut line_index = 0_usize;
    while start < source.len() {
        let remaining = source
            .get(start..)
            .ok_or(JsonlRecordSplitFailure::PositionExhausted)?;
        let newline = remaining.iter().position(|byte| *byte == b'\n');
        let (end, terminal) = match newline {
            Some(offset) => (
                start
                    .checked_add(offset)
                    .ok_or(JsonlRecordSplitFailure::PositionExhausted)?,
                false,
            ),
            None => (source.len(), true),
        };
        let record_end = if !terminal && end > start && source.get(end - 1) == Some(&b'\r') {
            end - 1
        } else {
            end
        };
        records.push(JsonlRecord {
            line: one_based_ordinal(line_index)
                .ok_or(JsonlRecordSplitFailure::PositionExhausted)?,
            bytes: source
                .get(start..record_end)
                .ok_or(JsonlRecordSplitFailure::PositionExhausted)?,
        });
        if terminal {
            break;
        }
        start = end
            .checked_add(1)
            .ok_or(JsonlRecordSplitFailure::PositionExhausted)?;
        line_index = line_index
            .checked_add(1)
            .ok_or(JsonlRecordSplitFailure::PositionExhausted)?;
    }
    Ok(records)
}

/// Parses one complete JSON record without discarding source structure.
pub fn parse_record(source: &[u8]) -> Result<ImportedStructuredValue, JsonFailure> {
    let source = str::from_utf8(source).map_err(|_| JsonFailure::InvalidUtf8)?;
    let mut deserializer = serde_json::Deserializer::from_str(source);
    deserializer.disable_recursion_limit();
    let raw = <&RawValue>::deserialize(&mut deserializer).map_err(|_| JsonFailure::Syntax)?;
    deserializer.end().map_err(|_| JsonFailure::Syntax)?;
    parse_raw(raw, 0)
}

fn parse_raw(raw: &RawValue, depth: usize) -> Result<ImportedStructuredValue, JsonFailure> {
    let source = raw.get().trim();
    match source.as_bytes().first() {
        Some(b'n') => Ok(ImportedStructuredValue::Null),
        Some(b't' | b'f') => serde_json::from_str(source)
            .map(ImportedStructuredValue::Boolean)
            .map_err(|_| JsonFailure::Syntax),
        Some(b'"') => serde_json::from_str::<String>(source)
            .map(|value| ImportedStructuredValue::String(ImportedText::new(value)))
            .map_err(|_| JsonFailure::Syntax),
        Some(b'[') => parse_container(source, ArraySeed { depth }),
        Some(b'{') => parse_container(source, ObjectSeed { depth }),
        Some(b'-' | b'0'..=b'9') => ImportedJsonNumber::try_new(source.to_owned())
            .map(ImportedStructuredValue::Number)
            .map_err(|_| JsonFailure::Syntax),
        _ => Err(JsonFailure::Syntax),
    }
}

fn parse_container<'de, S>(source: &'de str, seed: S) -> Result<S::Value, JsonFailure>
where
    S: DeserializeSeed<'de>,
{
    let mut deserializer = serde_json::Deserializer::from_str(source);
    deserializer.disable_recursion_limit();
    let value = seed
        .deserialize(&mut deserializer)
        .map_err(classify_serde_failure)?;
    deserializer.end().map_err(|_| JsonFailure::Syntax)?;
    Ok(value)
}

fn classify_serde_failure(error: serde_json::Error) -> JsonFailure {
    if error
        .to_string()
        .starts_with("JSON record exceeds the nesting limit")
    {
        JsonFailure::DepthExceeded
    } else {
        JsonFailure::Syntax
    }
}

struct ValueSeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for ValueSeed {
    type Value = ImportedStructuredValue;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = <&RawValue>::deserialize(deserializer)?;
        parse_raw(raw, self.depth).map_err(serde::de::Error::custom)
    }
}

struct ArraySeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for ArraySeed {
    type Value = ImportedStructuredValue;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.depth >= MAX_CONTAINER_DEPTH {
            return Err(serde::de::Error::custom(JsonFailure::DepthExceeded));
        }
        deserializer.deserialize_seq(ArrayVisitor { depth: self.depth })
    }
}

struct ArrayVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for ArrayVisitor {
    type Value = ImportedStructuredValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(ValueSeed {
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(ImportedStructuredValue::Array(values.into_boxed_slice()))
    }
}

struct ObjectSeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for ObjectSeed {
    type Value = ImportedStructuredValue;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.depth >= MAX_CONTAINER_DEPTH {
            return Err(serde::de::Error::custom(JsonFailure::DepthExceeded));
        }
        deserializer.deserialize_map(ObjectVisitor { depth: self.depth })
    }
}

struct ObjectVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for ObjectVisitor {
    type Value = ImportedStructuredValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut members = Vec::new();
        while let Some(name) = map.next_key::<String>()? {
            let value = map.next_value_seed(ValueSeed {
                depth: self.depth + 1,
            })?;
            members.push(ImportedStructuredObjectMember::new(
                ImportedText::new(name),
                value,
            ));
        }
        Ok(ImportedStructuredValue::Object(members.into_boxed_slice()))
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{ImportedStructuredValue, ImportedText};

    use super::{
        JsonFailure, JsonlRecordSplitFailure, one_based_ordinal, parse_record, split_jsonl_records,
    };

    const FIRST_RECORD: &[u8] = b"first";
    const FINAL_RECORD: &[u8] = b"last";

    #[test]
    fn s28_inv038_preserves_object_member_order() {
        let parsed = parse_record(br#"{"first":0,"second":1}"#).expect("synthetic JSON is valid");
        let ImportedStructuredValue::Object(members) = &parsed else {
            panic!("synthetic root should be an object");
        };
        assert_eq!(members[0].name().as_str(), "first");
        assert_eq!(members[1].name().as_str(), "second");
    }

    #[test]
    fn s28_inv038_preserves_duplicate_object_members() {
        let parsed = parse_record(br#"{"same":0,"same":1}"#).expect("synthetic JSON is valid");
        let ImportedStructuredValue::Object(members) = &parsed else {
            panic!("synthetic root should be an object");
        };

        assert_eq!(members.len(), 2);
        assert_eq!(members[0].name().as_str(), "same");
        assert_eq!(members[1].name().as_str(), "same");
    }

    #[test]
    fn s28_inv038_preserves_json_number_spelling() {
        let parsed = parse_record(br#"{"number":1e+09}"#).expect("synthetic JSON is valid");
        let ImportedStructuredValue::Object(members) = &parsed else {
            panic!("synthetic root should be an object");
        };
        let ImportedStructuredValue::Number(number) = members[0].value() else {
            panic!("synthetic number should be a number");
        };

        assert_eq!(number.as_str(), "1e+09");
    }

    #[test]
    fn s28_inv038_decodes_json_unicode_escapes() {
        let parsed = parse_record(br#"{"text":"\u0000"}"#).expect("synthetic JSON is valid");
        let ImportedStructuredValue::Object(members) = &parsed else {
            panic!("synthetic root should be an object");
        };
        let ImportedStructuredValue::String(value) = members[0].value() else {
            panic!("synthetic text should be a string");
        };

        assert_eq!(value, &ImportedText::new(String::from("\0")));
    }

    #[test]
    fn s28_inv038_counts_top_level_object_as_first_container() {
        let accepted = format!("{{\"nested\":{}0{}}}", "[".repeat(127), "]".repeat(127));
        let rejected = format!("{{\"nested\":{}0{}}}", "[".repeat(128), "]".repeat(128));
        assert!(parse_record(accepted.as_bytes()).is_ok());
        assert_eq!(
            parse_record(rejected.as_bytes()),
            Err(JsonFailure::DepthExceeded)
        );
    }

    #[test]
    fn s28_inv038_preserves_blank_record_for_reporting() {
        let records = split_jsonl_records(b"\nlast")
            .expect("the bounded synthetic JSONL positions are representable");

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].line(), 1);
        assert!(records[0].bytes().is_empty());
        assert_eq!(records[1].bytes(), FINAL_RECORD);
    }

    #[test]
    fn s28_inv038_strips_crlf_delimiter_from_record_bytes() {
        let records = split_jsonl_records(b"first\r\n")
            .expect("the bounded synthetic JSONL positions are representable");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].line(), 1);
        assert_eq!(records[0].bytes(), FIRST_RECORD);
    }

    #[test]
    fn s28_inv038_preserves_unterminated_final_record() {
        let records = split_jsonl_records(b"first\nlast")
            .expect("the bounded synthetic JSONL positions are representable");

        assert_eq!(records.len(), 2);
        assert_eq!(records[1].line(), 2);
        assert_eq!(records[1].bytes(), FINAL_RECORD);
    }

    #[test]
    fn s28_inv038_converts_zero_based_index_to_one_based_ordinal() {
        assert_eq!(one_based_ordinal(0), Some(1));
    }

    #[test]
    fn s28_inv038_terminal_delimiter_does_not_create_an_extra_record() {
        let records = split_jsonl_records(b"first\n")
            .expect("the bounded synthetic JSONL positions are representable");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].bytes(), FIRST_RECORD);
    }

    #[test]
    fn s28_inv038_empty_source_contains_no_records() {
        let records =
            split_jsonl_records(b"").expect("the empty source requires no representable position");

        assert!(records.is_empty());
    }

    #[test]
    fn s28_inv038_split_failure_display_is_content_silent() {
        assert_eq!(
            JsonlRecordSplitFailure::PositionExhausted.to_string(),
            "JSONL record splitting failed"
        );
    }

    #[test]
    fn s28_inv038_distinguishes_invalid_utf8_from_truncated_json() {
        assert_eq!(parse_record(b"\xff"), Err(JsonFailure::InvalidUtf8));
        assert_eq!(
            parse_record(br#"{"type":"event""#),
            Err(JsonFailure::Syntax)
        );
    }
}
