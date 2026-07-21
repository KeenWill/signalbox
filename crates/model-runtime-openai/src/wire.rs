//! OpenAI Chat Completions API wire types.
//!
//! Written from the provider's public Chat Completions documentation:
//! request and response bodies for `POST /v1/chat/completions`, the error
//! envelope, and the streaming chunk payloads. Response types tolerate
//! unknown fields (serde's default) so additive provider changes do not
//! break deserialization; unknown tool types are handled explicitly where
//! they are interpreted.

use serde::{Deserialize, Serialize};

// --- Request ---

#[derive(Debug, Serialize)]
pub(crate) struct ChatRequest {
    pub model: String,
    pub messages: Vec<WireChatMessage>,
    pub max_completion_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireFunctionTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

#[derive(Debug, Serialize)]
pub(crate) struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireChatMessage {
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<WireRequestToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireRequestToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireRequestFunction,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireRequestFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireFunctionTool {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireFunctionDefinition,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireFunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// --- Response ---

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletion {
    pub id: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub choices: Vec<ChatChoice>,
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatChoice {
    #[serde(default)]
    pub index: u32,
    pub message: Option<ChatResponseMessage>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatResponseMessage {
    pub role: Option<String>,
    pub content: Option<String>,
    pub refusal: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<WireResponseToolCall>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireResponseToolCall {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub function: Option<WireResponseFunction>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireResponseFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PromptTokensDetails {
    pub cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorEnvelope {
    pub error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireError {
    pub message: Option<String>,
    #[serde(rename = "type")]
    pub error_type: Option<String>,
    #[serde(default)]
    pub code: Option<serde_json::Value>,
}

impl WireError {
    /// The native code as text, when one was carried.
    pub(crate) fn code_text(&self) -> Option<String> {
        match &self.code {
            Some(serde_json::Value::String(code)) => Some(code.clone()),
            Some(serde_json::Value::Number(code)) => Some(code.to_string()),
            _ => None,
        }
    }

    /// Retains the native error material verbatim as neutral evidence.
    pub(crate) fn into_native_facts(self) -> signalbox_model_runtime::NativeErrorFacts {
        let code = self.code_text();
        signalbox_model_runtime::NativeErrorFacts {
            error_token: self.error_type,
            error_code: code,
            message: self.message,
        }
    }
}

// --- Streaming chunk payloads ---

#[derive(Debug, Deserialize)]
pub(crate) struct ChatChunk {
    pub id: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    pub usage: Option<WireUsage>,
    /// Some gateways deliver a terminal error as a data record; when
    /// present, the chunk is a definitive provider error.
    pub error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChunkChoice {
    #[serde(default)]
    pub index: u32,
    pub delta: Option<ChunkDelta>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChunkDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    pub refusal: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ChunkToolCall>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChunkToolCall {
    #[serde(default)]
    pub index: u32,
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub function: Option<ChunkFunction>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChunkFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
}
