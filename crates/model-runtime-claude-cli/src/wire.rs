//! Claude Code streamed-JSON wire types.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct SystemInit {
    pub(crate) session_id: String,
    pub(crate) tools: Vec<String>,
    pub(crate) mcp_servers: Vec<McpServerStatus>,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) slash_commands: Vec<String>,
    #[serde(default)]
    pub(crate) skills: Vec<String>,
    #[serde(default)]
    pub(crate) plugins: Vec<serde_json::Value>,
    pub(crate) claude_code_version: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct McpServerStatus {
    pub(crate) name: String,
    pub(crate) status: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssistantEvent {
    pub(crate) message: AssistantMessage,
    #[serde(default)]
    pub(crate) parent_tool_use_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssistantRawEvent {
    pub(crate) message: AssistantRawMessage,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssistantRawMessage {
    pub(crate) content: Vec<Box<serde_json::value::RawValue>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawToolUse {
    pub(crate) input: Box<serde_json::value::RawValue>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssistantMessage {
    pub(crate) model: String,
    pub(crate) id: String,
    pub(crate) role: String,
    pub(crate) content: Vec<AssistantContent>,
    #[serde(default)]
    pub(crate) usage: Option<MessageUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AssistantContent {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UserEvent {
    pub(crate) message: UserMessage,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UserMessage {
    pub(crate) role: String,
    pub(crate) content: Vec<UserContent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UserContent {
    #[serde(rename = "type")]
    pub(crate) content_type: String,
    pub(crate) tool_use_id: String,
    pub(crate) content: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResultEvent {
    pub(crate) subtype: String,
    pub(crate) is_error: bool,
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) stop_reason: Option<String>,
    #[serde(default)]
    pub(crate) terminal_reason: Option<String>,
    #[serde(default)]
    pub(crate) result: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<ResultUsage>,
    #[serde(default)]
    pub(crate) api_error_status: Option<u16>,
    #[serde(default)]
    pub(crate) errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct MessageUsage {
    #[serde(default)]
    pub(crate) input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) output_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct ResultUsage {
    #[serde(default)]
    pub(crate) input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) output_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u64>,
}
