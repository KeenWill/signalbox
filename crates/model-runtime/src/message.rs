//! Provider-neutral conversation vocabulary.
//!
//! The caller supplies conversation history as typed messages; adapters
//! translate them to the provider's wire shape. Assistant response material
//! comes back as [`AssistantPart`] values inside terminal evidence, in
//! provider order. These are Layer-1 values (docs/spec/runtime-substrate.md):
//! the caller maps them into its own durable representations and never
//! stores them as canonical records.

use crate::tool::{ToolCallId, ToolCallProposal, ToolName};

/// Who authored a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationRole {
    /// The end user or the caller acting for it.
    User,
    /// The assistant, replaying earlier model output.
    Assistant,
}

/// One conversation message: a role and its ordered parts.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationMessage {
    /// Who authored the message.
    pub role: ConversationRole,
    /// The message's ordered parts.
    pub parts: Vec<MessagePart>,
}

impl ConversationMessage {
    /// A user-role message containing one text part.
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: ConversationRole::User,
            parts: vec![MessagePart::Text(text.into())],
        }
    }

    /// An assistant message containing one text part.
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::Text(text.into())],
        }
    }
}

/// One part of a conversation message.
#[derive(Debug, Clone, PartialEq)]
pub enum MessagePart {
    /// Caller-authenticated durable image reference awaiting bounded materialization.
    ImageReference(crate::ImageReference),
    /// Materialized image bytes, encoded without local format interpretation.
    Image(crate::ImageInput),
    /// Plain text.
    Text(String),
    /// A tool call the assistant proposed in an earlier response, replayed
    /// as history.
    ToolCall(ToolCallProposal),
    /// The caller-produced result of an earlier tool call.
    ToolResult(ToolResultRecord),
    /// Reasoning from an earlier response. Adapters replay supported signed
    /// blocks and drop thinking they cannot replay.
    Thinking {
        /// The reasoning text.
        text: String,
        /// The provider integrity signature over the reasoning, when one
        /// was reported.
        signature: Option<String>,
    },
    /// Redacted reasoning from an earlier response, replayed where supported.
    RedactedThinking {
        /// The opaque provider payload.
        data: String,
    },
    /// One complete encrypted provider reasoning item retained for exact replay.
    ProviderReasoning {
        /// The complete provider output item as raw JSON.
        item_json: String,
        /// Effective runtime target used by the producing call.
        producing_target: crate::ResolvedTarget,
        /// Non-secret credential reference pinned by the producing call.
        producing_credential: crate::CredentialReference,
    },
    /// An opaque provider-produced compaction block replayed unchanged.
    ProviderCompaction {
        /// The complete provider content block as raw JSON.
        block_json: String,
    },
}

/// The caller-produced result of one earlier tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultRecord {
    /// The proposal this result answers.
    pub tool_call_id: ToolCallId,
    /// The result content, as text.
    pub content: String,
    /// Whether the caller reports the tool run as failed.
    pub is_error: bool,
}

/// One part of an assistant response, in provider order.
#[derive(Debug, Clone, PartialEq)]
pub enum AssistantPart {
    /// Response text.
    Text(String),
    /// Provider-visible reasoning text.
    Thinking {
        /// The reasoning text.
        text: String,
        /// A provider integrity signature over the reasoning, when reported.
        signature: Option<String>,
    },
    /// Reasoning the provider withheld and returned only in opaque form.
    RedactedThinking {
        /// The opaque provider payload, retained verbatim.
        data: String,
    },
    /// One complete encrypted provider reasoning item retained for exact replay.
    ProviderReasoning {
        /// The complete provider output item as raw JSON.
        item_json: String,
    },
    /// An opaque provider-produced compaction block, retained for replay.
    ProviderCompaction {
        /// The complete provider content block as raw JSON.
        block_json: String,
    },
    /// A proposed tool call. Decoding it into typed arguments is
    /// [`crate::decode_tool_arguments`]; executing it is never this layer's
    /// work.
    ToolCall(ToolCallProposal),
    /// A tool call whose argument object the CLI credential boundary suppressed
    /// as a whole. The admitted tool name remains available so the application
    /// can record a correlated non-executable denial.
    SuppressedToolCall(ToolName),
}
