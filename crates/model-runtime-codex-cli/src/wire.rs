//! Codex JSONL and adapter-envelope wire types.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct ThreadStarted {
    pub(crate) thread_id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TurnCompleted {
    pub(crate) usage: Usage,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TurnFailed {
    pub(crate) error: ThreadError,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ThreadError {
    pub(crate) message: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Usage {
    #[serde(default)]
    pub(crate) input_tokens: Option<i64>,
    #[serde(default)]
    pub(crate) cached_input_tokens: Option<i64>,
    #[serde(default)]
    pub(crate) cache_write_input_tokens: Option<i64>,
    #[serde(default)]
    pub(crate) output_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ItemEvent {
    pub(crate) item: Item,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ItemLifecycleEvent {
    pub(crate) item: ItemIdentity,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ItemIdentity {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) item_type: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Item {
    pub(crate) id: String,
    #[serde(flatten)]
    pub(crate) details: ItemDetails,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ItemDetails {
    AgentMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Error {
        message: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelEnvelope {
    pub(crate) outcome: EnvelopeOutcome,
    pub(crate) text: String,
    pub(crate) tool_calls: Vec<EnvelopeToolCall>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EnvelopeOutcome {
    Completed,
    Refused,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EnvelopeToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    /// The provider's tool-argument text, carried as JSON **text** rather than
    /// as inline JSON.
    ///
    /// Strict structured output rejects a free-form object — every object in
    /// the schema must supply `additionalProperties: false` and require all
    /// its properties — so unconstrained tool arguments are not expressible
    /// as an object member. The envelope therefore carries the arguments
    /// inside a string. The schema requests an argument object, but the
    /// adapter does not hold the text to that shape: it checks only the shared
    /// JSON nesting bound and hands the text onward byte-verbatim when it is
    /// credential-shape clean, so malformed and non-object text reaches the
    /// caller as proposal material for the shared typed decoders to classify.
    pub(crate) arguments: String,
}

/// Schema passed to `codex exec --output-schema`.
///
/// Every object supplies `additionalProperties: false` and requires all its
/// properties, which strict structured-output validation demands of every
/// object node; the live API rejects any other shape as
/// `invalid_json_schema`. That rule is also why `arguments` is a string
/// carrying JSON text (see [`EnvelopeToolCall::arguments`]). Semantic
/// constraints involving the caller's declared tools are checked by the
/// adapter after decoding.
pub(crate) const OUTPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "outcome": {
      "type": "string",
      "enum": ["completed", "refused"]
    },
    "text": {
      "type": "string"
    },
    "tool_calls": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "id": {
            "type": "string"
          },
          "name": {
            "type": "string"
          },
          "arguments": {
            "type": "string",
            "description": "The tool's argument object, encoded as a JSON string"
          }
        },
        "required": ["id", "name", "arguments"],
        "additionalProperties": false
      }
    }
  },
  "required": ["outcome", "text", "tool_calls"],
  "additionalProperties": false
}"#;
