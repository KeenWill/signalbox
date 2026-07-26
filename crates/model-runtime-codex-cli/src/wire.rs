//! Codex JSONL and adapter-envelope wire types.

use serde::Deserialize;
use serde_json::value::RawValue;

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
    pub(crate) input_tokens: i64,
    #[serde(default)]
    pub(crate) cached_input_tokens: i64,
    #[serde(default)]
    pub(crate) cache_write_input_tokens: i64,
    pub(crate) output_tokens: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ItemEvent {
    pub(crate) item: Item,
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
    pub(crate) arguments: Box<RawValue>,
}

/// Schema passed to `codex exec --output-schema`.
///
/// Every field is required to remain compatible with strict structured-output
/// validation. Semantic constraints involving the caller's declared tools are
/// checked by the adapter after decoding.
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
            "type": "object",
            "additionalProperties": true
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
