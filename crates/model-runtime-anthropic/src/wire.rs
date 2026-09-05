//! Anthropic Messages API wire types.
//!
//! Written from the provider's public Messages API documentation: request and
//! response bodies for `POST /v1/messages`, the error envelope, and the SSE
//! streaming event payloads. Response types tolerate unknown fields (serde's
//! default) so additive provider changes do not break deserialization;
//! unknown content-block and event *types* are handled explicitly where they
//! are interpreted.

use serde::{Deserialize, Deserializer, Serialize};

pub(crate) fn raw_json_is_object(raw: &serde_json::value::RawValue) -> bool {
    raw.get().bytes().find(|byte| !byte.is_ascii_whitespace()) == Some(b'{')
}

// --- Request ---

/// Exact request accepted by `POST /v1/messages`.
///
/// No sampling member and no top-level `thinking` member: the provider answers
/// either with a 400, and omitting the parameter is the accepted form.
/// Replayed thinking travels as a content block inside `messages`.
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
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<WireToolChoice>,
    pub context_management: ContextManagement,
    pub stream: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ContextManagement {
    pub edits: Vec<ContextManagementEdit>,
}

impl ContextManagement {
    pub(crate) fn for_target(server_compaction: bool) -> Self {
        let mut edits = vec![ContextManagementEdit::ClearToolUses];
        if server_compaction {
            edits.push(ContextManagementEdit::Compact);
        }
        Self { edits }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(crate) enum ContextManagementEdit {
    #[serde(rename = "clear_tool_uses_20250919")]
    ClearToolUses,
    #[serde(rename = "compact_20260112")]
    Compact,
}

#[derive(Debug, Serialize)]
pub(crate) struct OutputConfig {
    pub effort: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireMessage {
    pub role: &'static str,
    pub content: Vec<WireRequestBlock>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum WireRequestBlock {
    Known(WireKnownRequestBlock),
    ProviderCompaction(Box<serde_json::value::RawValue>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(crate) enum WireKnownRequestBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Box<serde_json::value::RawValue>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String, signature: String },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

#[derive(Debug, Serialize)]
pub(crate) struct WireTool {
    pub name: String,
    pub description: String,
    pub input_schema: Box<serde_json::value::RawValue>,
}

/// The only `tool_choice` shape this adapter emits.
///
/// The forced shapes — `{"type":"any"}` and `{"type":"tool","name":…}` — are
/// unrepresentable because the provider answers both with a 400.
/// `disable_parallel_tool_use` alongside `auto` still admits at most one call.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(crate) enum WireToolChoice {
    #[serde(rename = "auto")]
    Auto {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
}

/// Exact request accepted by `POST /v1/messages/count_tokens`.
#[derive(Debug, Serialize)]
pub(crate) struct CountTokensRequest {
    pub model: String,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<WireToolChoice>,
    pub context_management: ContextManagement,
}

impl From<MessagesRequest> for CountTokensRequest {
    fn from(request: MessagesRequest) -> Self {
        Self {
            model: request.model,
            messages: request.messages,
            system: request.system,
            output_config: request.output_config,
            speed: request.speed,
            tools: request.tools,
            tool_choice: request.tool_choice,
            context_management: request.context_management,
        }
    }
}

/// Complete successful count-tokens response.
#[derive(Debug, Deserialize)]
pub(crate) struct CountTokensResponse {
    pub input_tokens: u64,
}

// --- Response ---

#[derive(Debug, Deserialize)]
pub(crate) struct MessagesResponse {
    #[serde(rename = "type")]
    pub response_type: Option<String>,
    pub role: Option<String>,
    pub id: Option<String>,
    pub model: Option<String>,
    pub content: Vec<Box<serde_json::value::RawValue>>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: Option<WireUsage>,
}

/// A parsed response content block.
///
/// Blocks are hand-dispatched on their `type` tag from raw JSON slices —
/// rather than via an internally tagged serde enum — so a `tool_use`
/// block's `input` stays the provider's verbatim raw JSON: serde's tagged
/// representation buffers content and cannot expose raw slices.
#[derive(Debug)]
pub(crate) enum WireResponseBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        /// The provider's raw JSON slice, verbatim.
        input: Box<serde_json::value::RawValue>,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    Compaction {
        /// The provider's complete content block, retained verbatim.
        raw: Box<serde_json::value::RawValue>,
    },
    /// The provider's server-side fallback marker: the point in this
    /// response where one model declined and another continued.
    ///
    /// Recognized explicitly rather than left unknown so a genuine
    /// cross-model substitution is distinguishable from additive provider
    /// evolution. This adapter never enables server-side fallback, so
    /// observing the block is evidence that the served model is not the
    /// resolved target.
    Fallback {
        /// The model the block names as continuing the turn, when it named
        /// one.
        to_model: Option<String>,
    },
    /// A content-block type this adapter does not recognize. Surfaced as
    /// evidence rather than silently dropped: response material containing
    /// unknown parts is not valid completion material.
    Unrecognized,
}

/// Parses one content block from its raw JSON slice.
pub(crate) fn parse_response_block(
    raw: &serde_json::value::RawValue,
) -> Result<WireResponseBlock, serde_json::Error> {
    #[derive(Deserialize)]
    struct Tag {
        #[serde(rename = "type")]
        kind: String,
    }
    #[derive(Deserialize)]
    struct TextBlock {
        text: String,
    }
    #[derive(Deserialize)]
    struct ToolUseBlock {
        id: String,
        name: String,
        input: Box<serde_json::value::RawValue>,
    }
    #[derive(Deserialize)]
    struct ThinkingBlock {
        thinking: String,
        signature: Option<String>,
    }
    #[derive(Deserialize)]
    struct RedactedThinkingBlock {
        data: String,
    }
    #[derive(Deserialize)]
    struct CompactionBlock {
        content: serde_json::Value,
        encrypted_content: Option<serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct FallbackBlock {
        to: Option<FallbackModel>,
    }
    #[derive(Deserialize)]
    struct FallbackModel {
        model: Option<String>,
    }
    let tag: Tag = serde_json::from_str(raw.get())?;
    Ok(match tag.kind.as_str() {
        "text" => {
            let block: TextBlock = serde_json::from_str(raw.get())?;
            WireResponseBlock::Text { text: block.text }
        }
        "tool_use" => {
            let block: ToolUseBlock = serde_json::from_str(raw.get())?;
            WireResponseBlock::ToolUse {
                id: block.id,
                name: block.name,
                input: block.input,
            }
        }
        "thinking" => {
            let block: ThinkingBlock = serde_json::from_str(raw.get())?;
            WireResponseBlock::Thinking {
                thinking: block.thinking,
                signature: block.signature,
            }
        }
        "redacted_thinking" => {
            let block: RedactedThinkingBlock = serde_json::from_str(raw.get())?;
            WireResponseBlock::RedactedThinking { data: block.data }
        }
        "compaction" => {
            let block: CompactionBlock = serde_json::from_str(raw.get())?;
            let content_valid = matches!(block.content, serde_json::Value::Null)
                || block
                    .content
                    .as_str()
                    .is_some_and(|content| !content.is_empty());
            let encrypted_content_valid = block.encrypted_content.is_none_or(|content| {
                matches!(
                    content,
                    serde_json::Value::Null | serde_json::Value::String(_)
                )
            });
            if !content_valid || !encrypted_content_valid {
                return Err(<serde_json::Error as serde::de::Error>::custom(
                    "invalid compaction block",
                ));
            }
            WireResponseBlock::Compaction {
                raw: serde_json::value::RawValue::from_string(raw.get().to_owned())?,
            }
        }
        "fallback" => {
            let block: FallbackBlock = serde_json::from_str(raw.get())?;
            WireResponseBlock::Fallback {
                to_model: block.to.and_then(|to| to.model),
            }
        }
        _ => WireResponseBlock::Unrecognized,
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub iterations: Option<Vec<WireIterationUsage>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireIterationUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorEnvelope {
    #[serde(rename = "type")]
    pub envelope_type: String,
    pub error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireError {
    #[serde(rename = "type")]
    pub error_type: Option<String>,
    pub message: Option<String>,
}

impl WireError {
    /// Retains the native error material verbatim as neutral evidence. The
    /// Messages error envelope carries no code distinct from its type token.
    pub(crate) fn into_native_facts(self) -> signalbox_model_runtime::NativeErrorFacts {
        signalbox_model_runtime::NativeErrorFacts {
            error_token: self.error_type,
            error_code: None,
            message: self.message,
        }
    }
}

// --- Streaming event payloads ---

#[derive(Debug, Deserialize)]
pub(crate) struct MessageStartEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub message: MessagesResponse,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContentBlockStartEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub index: u32,
    pub content_block: Box<serde_json::value::RawValue>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContentBlockDeltaEvent {
    #[serde(rename = "type")]
    pub event_type: String,
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
    #[serde(rename = "compaction_delta")]
    Compaction {
        #[serde(default)]
        content: WireCompactionContent,
        encrypted_content: Option<String>,
    },
    /// A delta type this adapter does not recognize (the provider documents
    /// that new delta types may be added); tolerated and ignored.
    #[serde(other)]
    Unrecognized,
}

/// The required compaction-delta content field, preserving the distinction
/// between an explicit JSON null and a missing field.
#[derive(Debug, Default)]
pub(crate) enum WireCompactionContent {
    #[default]
    Missing,
    Null,
    Text(String),
}

impl<'de> Deserialize<'de> for WireCompactionContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = WireCompactionContent;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string or null compaction content value")
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WireCompactionContent::Null)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WireCompactionContent::Null)
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                String::deserialize(deserializer).map(WireCompactionContent::Text)
            }
        }

        deserializer.deserialize_option(Visitor)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContentBlockStopEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub index: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessageDeltaEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub delta: Option<MessageDeltaBody>,
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessageDeltaBody {
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum MessageStopEvent {
    #[serde(rename = "message_stop")]
    MessageStop,
}
