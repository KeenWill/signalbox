//! Tool arguments for `docs/spec/tool-loop.md`.

use serde::{
    Deserialize, Serialize, Serializer, de::IgnoredAny, ser::SerializeMap, ser::SerializeSeq,
};

const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;

/// Which bounded representation normalized tool arguments carry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolArgumentsKind {
    /// Compact JSON with recursively lexical object keys.
    Json,
    /// Exact provider text that did not decode as JSON.
    Undecodable,
}

/// One bounded normalized tool-argument value.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NormalizedToolArguments {
    pub(super) kind: ToolArgumentsKind,
    pub(super) value: String,
}

impl NormalizedToolArguments {
    /// Normalizes exact provider-supplied UTF-8 text.
    ///
    /// Syntactically valid JSON is compacted with recursively lexical object
    /// keys. Invalid JSON remains exact and is tagged `Undecodable`.
    pub fn try_from_provider_text(value: String) -> Result<Self, ToolArgumentsError> {
        if value.len() > MAX_TOOL_ARGUMENT_BYTES {
            return Err(ToolArgumentsError {
                failure: ToolArgumentsFailure::TooLarge { bytes: value.len() },
                value,
            });
        }
        if value.contains('\0') {
            return Err(ToolArgumentsError {
                failure: ToolArgumentsFailure::ContainsNull,
                value,
            });
        }

        // The byte cap does not bound JSON depth. Disabling serde_json's
        // recursion limit is safe here only because serde_stacker grows the
        // parse and serialization stacks and iterative destruction below keeps
        // a deeply nested Value from overflowing the thread stack during Drop.
        if !is_complete_json(&value) {
            return Ok(Self {
                kind: ToolArgumentsKind::Undecodable,
                value,
            });
        }
        let mut deserializer = serde_json::Deserializer::from_str(&value);
        deserializer.disable_recursion_limit();
        let deserializer = serde_stacker::Deserializer::new(&mut deserializer);
        let decoded = match serde_json::Value::deserialize(deserializer) {
            Ok(decoded) => decoded,
            Err(_) => {
                return Err(ToolArgumentsError {
                    value,
                    failure: ToolArgumentsFailure::CanonicalizationFailed,
                });
            }
        };
        let mut canonical = Vec::with_capacity(value.len());
        let mut serializer = serde_json::Serializer::new(&mut canonical);
        let serializer = serde_stacker::Serializer::new(&mut serializer);
        let serialization = LexicallyOrderedJson(&decoded).serialize(serializer);
        drop_json_value_iteratively(decoded);
        serialization.map_err(|_| ToolArgumentsError {
            value: value.clone(),
            failure: ToolArgumentsFailure::CanonicalizationFailed,
        })?;
        let value = String::from_utf8(canonical).map_err(|_| ToolArgumentsError {
            value,
            failure: ToolArgumentsFailure::CanonicalizationFailed,
        })?;
        if value.len() > MAX_TOOL_ARGUMENT_BYTES {
            return Err(ToolArgumentsError {
                failure: ToolArgumentsFailure::CanonicalTooLarge { bytes: value.len() },
                value,
            });
        }
        Ok(Self {
            kind: ToolArgumentsKind::Json,
            value,
        })
    }

    /// Reconstitutes one stored normalized value, rejecting representation
    /// drift or a false kind tag.
    pub fn try_from_stored(
        kind: ToolArgumentsKind,
        value: String,
    ) -> Result<Self, ToolArgumentsError> {
        let normalized = Self::try_from_provider_text(value.clone())?;
        if normalized.kind != kind {
            return Err(ToolArgumentsError {
                value,
                failure: ToolArgumentsFailure::StoredKindMismatch,
            });
        }
        if normalized.value != value {
            return Err(ToolArgumentsError {
                value,
                failure: ToolArgumentsFailure::StoredJsonNotCanonical,
            });
        }
        Ok(normalized)
    }

    /// Returns the closed representation tag.
    pub const fn kind(&self) -> ToolArgumentsKind {
        self.kind
    }

    /// Borrows the canonical JSON or exact undecodable text.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Returns the tag and stored text.
    pub fn into_parts(self) -> (ToolArgumentsKind, String) {
        (self.kind, self.value)
    }
}

/// A stacker-compatible view that restores the lexical object ordering which
/// canonical tool JSON promises independently of serde_json's map backend.
struct LexicallyOrderedJson<'a>(&'a serde_json::Value);

impl Serialize for LexicallyOrderedJson<'_> {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        match self.0 {
            serde_json::Value::Null => serializer.serialize_unit(),
            serde_json::Value::Bool(value) => serializer.serialize_bool(*value),
            serde_json::Value::Number(value) => value.serialize(serializer),
            serde_json::Value::String(value) => serializer.serialize_str(value),
            serde_json::Value::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&Self(value))?;
                }
                sequence.end()
            }
            serde_json::Value::Object(object) => {
                let mut entries = object.iter().collect::<Vec<_>>();
                entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, &Self(value))?;
                }
                map.end()
            }
        }
    }
}

fn is_complete_json(value: &str) -> bool {
    let mut deserializer = serde_json::Deserializer::from_str(value);
    deserializer.disable_recursion_limit();
    let decoded = {
        let deserializer = serde_stacker::Deserializer::new(&mut deserializer);
        IgnoredAny::deserialize(deserializer)
    };
    decoded.is_ok() && deserializer.end().is_ok()
}

fn drop_json_value_iteratively(value: serde_json::Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::Array(mut values) => pending.append(&mut values),
            serde_json::Value::Object(values) => pending.extend(values.into_values()),
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => {}
        }
    }
}

/// Why tool-argument normalization or reconstitution failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolArgumentsFailure {
    /// Provider text exceeded the admission bound.
    TooLarge {
        /// The observed UTF-8 byte count.
        bytes: usize,
    },
    /// Canonical JSON exceeded the admission bound.
    CanonicalTooLarge {
        /// The canonical UTF-8 byte count.
        bytes: usize,
    },
    /// Provider text contained U+0000, which cannot enter durable text.
    ContainsNull,
    /// Serialization of an already-decoded JSON value unexpectedly failed.
    CanonicalizationFailed,
    /// The stored tag disagreed with whether the text decodes as JSON.
    StoredKindMismatch,
    /// Stored JSON was not in its canonical compact representation.
    StoredJsonNotCanonical,
}

/// Failed argument construction retaining the rejected text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolArgumentsError {
    value: String,
    failure: ToolArgumentsFailure,
}

impl ToolArgumentsError {
    /// Borrows the rejected text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the exact normalization failure.
    pub const fn failure(&self) -> ToolArgumentsFailure {
        self.failure
    }

    /// Returns the rejected text and failure.
    pub fn into_parts(self) -> (String, ToolArgumentsFailure) {
        (self.value, self.failure)
    }
}
