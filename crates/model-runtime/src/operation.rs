//! The one explicitly authorized model operation.

use crate::message::ConversationMessage;
use crate::output::StructuredOutputContract;
use crate::settings::ModelSettings;
use crate::target::{RequestedTarget, ResolvedTarget};
use crate::tool::{ToolDefinition, ToolName};

/// Whether the caller wants the exchange delivered as one buffered response
/// or as a provider event stream.
///
/// Either way the adapter performs at most one provider interaction and
/// reports the same terminal-evidence vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// One buffered response body.
    Buffered,
    /// The provider's event stream, surfaced as observations.
    Streamed,
}

/// How the provider may choose among the declared tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// The model decides whether to call a declared tool.
    Automatic,
    /// The model must call some declared tool.
    AnyTool,
    /// The model must call the named tool.
    Named(ToolName),
}

/// One explicitly authorized model operation.
///
/// The correlation value `C` is the caller's durable operation identity
/// (Signalbox's `ModelCallId`, or any other caller identity), opaque to this
/// layer. Adapters thread it onto every observation and the terminal report
/// (ADR-0005: a runtime-generated identity is never authoritative
/// correlation).
///
/// One operation authorizes at most one provider interaction. Nothing in
/// this layer turns one operation into two requests; a retry or continuation
/// is a new operation with a new caller identity.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelOperation<C> {
    /// The caller's durable identity for this operation, threaded onto every
    /// observation and evidence record.
    pub correlation: C,
    /// The caller's original model selection, for provenance.
    pub requested_target: RequestedTarget,
    /// The exact hub-resolved model identifier this operation must use.
    pub resolved_target: ResolvedTarget,
    /// System instructions, when the caller supplies any.
    pub system: Option<String>,
    /// Conversation history, oldest first.
    pub messages: Vec<ConversationMessage>,
    /// Sampling and limit settings.
    pub settings: ModelSettings,
    /// Tools the model may propose calling.
    pub tools: Vec<ToolDefinition>,
    /// How the provider may choose among the declared tools; ignored by
    /// adapters when `tools` is empty and no output contract is set.
    pub tool_choice: ToolChoice,
    /// A structured-output contract the response must satisfy, when the
    /// caller demands typed output.
    pub output_contract: Option<StructuredOutputContract>,
    /// Buffered or streamed delivery.
    pub delivery: DeliveryMode,
}

impl<C> ModelOperation<C> {
    /// An operation carrying the required facts; optional facts start empty
    /// (no system prompt, no tools, automatic tool choice, no output
    /// contract, buffered delivery).
    pub fn new(
        correlation: C,
        requested_target: RequestedTarget,
        resolved_target: ResolvedTarget,
        messages: Vec<ConversationMessage>,
        settings: ModelSettings,
    ) -> Self {
        Self {
            correlation,
            requested_target,
            resolved_target,
            system: None,
            messages,
            settings,
            tools: Vec::new(),
            tool_choice: ToolChoice::Automatic,
            output_contract: None,
            delivery: DeliveryMode::Buffered,
        }
    }
}
