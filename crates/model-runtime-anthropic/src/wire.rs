//! Anthropic Messages API wire types.
//!
//! Written from the provider's public Messages API documentation: request and
//! response bodies for `POST /v1/messages`, the error envelope, and the SSE
//! streaming event payloads. Response types tolerate unknown fields (serde's
//! default) so additive provider changes do not break deserialization;
//! unknown content-block and event *types* are handled explicitly where they
//! are interpreted.

use serde::{Deserialize, Serialize};

// --- Request ---

#[derive(Debug, Serialize)]
pub(crate) struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<WireToolChoice>,
    pub stream: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireMessage {
    pub role: &'static str,
    pub content: Vec<WireRequestBlock>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(crate) enum WireRequestBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

#[derive(Debug, Serialize)]
pub(crate) struct WireTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(crate) enum WireToolChoice {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "any")]
    Any,
    #[serde(rename = "tool")]
    Tool {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
}

// --- Response ---

#[derive(Debug, Deserialize)]
pub(crate) struct MessagesResponse {
    #[serde(rename = "type")]
    pub response_type: Option<String>,
    pub role: Option<String>,
    pub id: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub content: Vec<WireResponseBlock>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum WireResponseBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
    /// A content-block type this adapter does not recognize. Surfaced as
    /// evidence rather than silently dropped: response material containing
    /// unknown parts is not valid completion material.
    #[serde(other)]
    Unrecognized,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorEnvelope {
    pub error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireError {
    #[serde(rename = "type")]
    pub error_type: Option<String>,
    pub message: Option<String>,
}

impl WireError {
    /// Retains the native error material verbatim as neutral evidence.
    pub(crate) fn into_native_facts(self) -> signalbox_model_runtime::NativeErrorFacts {
        signalbox_model_runtime::NativeErrorFacts {
            error_token: self.error_type,
            message: self.message,
        }
    }
}

// --- Streaming event payloads ---

#[derive(Debug, Deserialize)]
pub(crate) struct MessageStartEvent {
    pub message: MessagesResponse,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContentBlockStartEvent {
    pub index: u32,
    pub content_block: WireResponseBlock,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContentBlockDeltaEvent {
    pub index: u32,
    pub delta: WireDelta,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum WireDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    /// A delta type this adapter does not recognize (the provider documents
    /// that new delta types may be added); tolerated and ignored.
    #[serde(other)]
    Unrecognized,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContentBlockStopEvent {
    pub index: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessageDeltaEvent {
    pub delta: Option<MessageDeltaBody>,
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessageDeltaBody {
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}
