//! Tool definitions, provider tool-call proposals, and typed decoding.
//!
//! This layer defines tools and decodes provider proposals into typed values.
//! It contains no execution machinery: a decoded proposal is data the caller
//! may turn into a durable, separately authorized tool request; nothing here
//! invokes anything.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::value::{RawValue, to_raw_value};

/// The name a tool is declared and proposed under.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolName(String);

impl ToolName {
    /// Wraps a tool name exactly as declared.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The declared name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The provider-issued identifier correlating a tool proposal with its
/// eventual result message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCallId(String);

impl ToolCallId {
    /// Wraps a provider-issued tool-call identifier exactly as observed.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The identifier as observed.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One declared tool: a name, a description for the model, and the JSON
/// Schema of its arguments.
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    /// The name the model proposes this tool under.
    pub name: ToolName,
    /// What the tool does, addressed to the model.
    pub description: String,
    /// JSON Schema for the tool's arguments object.
    pub input_schema: Box<RawValue>,
}

impl PartialEq for ToolDefinition {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.description == other.description
            && self.input_schema.get() == other.input_schema.get()
    }
}

impl ToolDefinition {
    /// Declares a tool whose argument schema is generated from `T`.
    pub fn of_type<T: JsonSchema>(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::with_schema(name, description, schemars::schema_for!(T).to_value())
    }

    /// Declares a tool with an explicit argument schema.
    pub fn with_schema(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
    ) -> Self {
        Self::with_raw_schema(
            name,
            description,
            to_raw_value(&input_schema).expect("serde_json::Value always serializes as JSON"),
        )
    }

    /// Declares a tool with an already validated, stack-safe raw schema.
    pub fn with_raw_schema(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Box<RawValue>,
    ) -> Self {
        Self {
            name: ToolName::new(name),
            description: description.into(),
            input_schema,
        }
    }
}

/// A provider's proposal to call one tool, decoded from the wire but not yet
/// typed.
///
/// The arguments stay as the raw JSON text the provider produced;
/// [`decode_tool_arguments`] turns them into a typed value. Executing the
/// proposal is never this layer's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallProposal {
    /// Provider-issued identifier for this proposal.
    pub id: ToolCallId,
    /// The tool the model proposes to call.
    pub name: ToolName,
    /// The proposed arguments as raw JSON text, exactly as produced.
    pub arguments_json: String,
}

/// Why a proposal's arguments did not decode into the requested type.
///
/// The two classes stay distinct so the caller can tell a provider that
/// produced non-JSON from one that produced well-formed JSON of the wrong
/// shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecodeFailure {
    /// The argument text is not syntactically valid JSON.
    JsonSyntax {
        /// The parser's rendered description of the syntax failure.
        detail: String,
    },
    /// The argument text is valid JSON that does not deserialize as the
    /// requested type.
    SchemaMismatch {
        /// The deserializer's rendered description of the mismatch.
        detail: String,
    },
}

impl std::fmt::Display for ToolDecodeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JsonSyntax { detail } => {
                write!(f, "tool arguments are not valid JSON: {detail}")
            }
            Self::SchemaMismatch { detail } => {
                write!(
                    f,
                    "tool arguments do not match the declared schema: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for ToolDecodeFailure {}

/// Decodes a proposal's arguments into the tool's typed argument value.
///
/// Pure parsing: dispatching on the proposal's name and deciding what to do
/// with the typed value — or with a failure — belongs to the caller.
pub fn decode_tool_arguments<T: DeserializeOwned>(
    proposal: &ToolCallProposal,
) -> Result<T, ToolDecodeFailure> {
    let value: serde_json::Value =
        serde_json::from_str(&proposal.arguments_json).map_err(|error| {
            ToolDecodeFailure::JsonSyntax {
                detail: error.to_string(),
            }
        })?;
    serde_json::from_value(value).map_err(|error| ToolDecodeFailure::SchemaMismatch {
        detail: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ToolCallId, ToolCallProposal, ToolDecodeFailure, ToolDefinition, ToolName,
        decode_tool_arguments,
    };
    use expect_test::expect;

    #[derive(Debug, PartialEq, serde::Deserialize, schemars::JsonSchema)]
    struct LookupArguments {
        city: String,
        limit: u32,
    }

    fn proposal(arguments_json: &str) -> ToolCallProposal {
        ToolCallProposal {
            id: ToolCallId::new("call_1"),
            name: ToolName::new("lookup"),
            arguments_json: arguments_json.to_string(),
        }
    }

    #[test]
    fn generated_schema_names_every_declared_argument() {
        let definition = ToolDefinition::of_type::<LookupArguments>("lookup", "Looks up a city.");

        assert_eq!(definition.name, ToolName::new("lookup"));
        expect![[r#"
            {
              "$schema": "https://json-schema.org/draft/2020-12/schema",
              "properties": {
                "city": {
                  "type": "string"
                },
                "limit": {
                  "format": "uint32",
                  "minimum": 0,
                  "type": "integer"
                }
              },
              "required": [
                "city",
                "limit"
              ],
              "title": "LookupArguments",
              "type": "object"
            }"#]]
        .assert_eq(&format!(
            "{:#}",
            serde_json::from_str::<serde_json::Value>(definition.input_schema.get())
                .expect("generated schema remains valid JSON")
        ));
    }

    #[test]
    fn well_formed_arguments_decode_to_the_typed_value() {
        let decoded: LookupArguments =
            decode_tool_arguments(&proposal(r#"{"city":"Oslo","limit":3}"#))
                .expect("well-formed arguments matching the schema decode");

        assert_eq!(
            decoded,
            LookupArguments {
                city: "Oslo".to_string(),
                limit: 3
            }
        );
    }

    #[test]
    fn non_json_arguments_fail_as_syntax_not_schema() {
        let failure = decode_tool_arguments::<LookupArguments>(&proposal("{not json"))
            .expect_err("malformed JSON must fail as a syntax class");

        assert!(matches!(failure, ToolDecodeFailure::JsonSyntax { .. }));
    }

    #[test]
    fn wrong_shape_arguments_fail_as_schema_not_syntax() {
        let failure = decode_tool_arguments::<LookupArguments>(&proposal(
            r#"{"city":"Oslo","limit":"three"}"#,
        ))
        .expect_err("well-formed JSON of the wrong shape must fail as a schema class");

        assert!(matches!(failure, ToolDecodeFailure::SchemaMismatch { .. }));
    }
}
