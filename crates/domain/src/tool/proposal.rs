//! Tool proposal for `docs/spec/tool-loop.md`.

use super::arguments::{NormalizedToolArguments, ToolArgumentsKind};
use super::name::ToolName;
use crate::{AssistantText, ProviderCompactionBlock, ProviderReasoningItem};

pub(super) const SUPPRESSED_TOOL_ARGUMENTS: &str = r#"{"redacted":"[redacted]"}"#;

/// Zero-based proposal order among tool calls in one model response.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolRequestOrdinal(u32);

impl ToolRequestOrdinal {
    /// Checks that one in-memory index fits the durable ordinal space.
    pub fn try_from_usize(value: usize) -> Option<Self> {
        u32::try_from(value).ok().map(Self)
    }

    /// Reconstitutes one stored zero-based ordinal.
    pub const fn from_u32(value: u32) -> Self {
        Self(value)
    }

    /// Returns the zero-based ordinal.
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// One normalized logical proposal from a completed model response.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolCallProposal {
    pub(super) name: ToolName,
    pub(super) arguments: NormalizedToolArguments,
    suppressed: bool,
    inadmissible_reason: Option<super::ToolInadmissibleReason>,
}

impl ToolCallProposal {
    /// Assembles already-checked provider-neutral content.
    pub const fn new(name: ToolName, arguments: NormalizedToolArguments) -> Self {
        Self {
            name,
            arguments,
            suppressed: false,
            inadmissible_reason: None,
        }
    }

    /// Constructs the inert projection of a proposal whose arguments were
    /// suppressed by a provider credential boundary.
    pub fn suppressed(name: ToolName) -> Self {
        Self {
            name,
            arguments: NormalizedToolArguments {
                kind: ToolArgumentsKind::Json,
                value: String::from(SUPPRESSED_TOOL_ARGUMENTS),
            },
            suppressed: true,
            inadmissible_reason: None,
        }
    }

    /// Constructs a request rejected during response admission, retaining its argument preview.
    pub const fn inadmissible(
        name: ToolName,
        arguments: NormalizedToolArguments,
        reason: super::ToolInadmissibleReason,
    ) -> Self {
        Self {
            name,
            arguments,
            suppressed: false,
            inadmissible_reason: Some(reason),
        }
    }

    /// Returns the request-level admission failure, when present.
    pub const fn inadmissible_reason(&self) -> Option<super::ToolInadmissibleReason> {
        self.inadmissible_reason
    }

    /// Borrows the checked tool name.
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Borrows normalized arguments.
    pub const fn arguments(&self) -> &NormalizedToolArguments {
        &self.arguments
    }

    /// Reports whether the proposal is an inert credential-boundary projection.
    pub const fn is_suppressed(&self) -> bool {
        self.suppressed
    }
}

/// One ordered assistant response part admitted by the tool-loop slice.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum AssistantResponsePart {
    /// Exact assistant text.
    Text(AssistantText),
    /// One opaque provider-produced compaction block.
    ProviderCompaction(ProviderCompactionBlock),
    /// One complete provider reasoning item retained for replay.
    ProviderReasoning(ProviderReasoningItem),
    /// One normalized logical tool proposal.
    ToolCall(ToolCallProposal),
}

/// A completed response proven to contain at least one tool proposal.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolUsingAssistantResponse {
    parts: Box<[AssistantResponsePart]>,
    tool_count: usize,
}

impl ToolUsingAssistantResponse {
    /// Checks the positive tool-count requirement while preserving
    /// part order.
    pub fn try_from_parts(
        parts: Vec<AssistantResponsePart>,
    ) -> Result<Self, ToolUsingAssistantResponseError> {
        let tool_count = parts
            .iter()
            .filter(|part| matches!(part, AssistantResponsePart::ToolCall(_)))
            .count();
        if tool_count == 0 {
            return Err(ToolUsingAssistantResponseError { parts });
        }
        Ok(Self {
            parts: parts.into_boxed_slice(),
            tool_count,
        })
    }

    /// Returns every response part in provider order.
    pub fn parts(&self) -> &[AssistantResponsePart] {
        &self.parts
    }

    /// Returns the positive number of tool proposals.
    pub const fn tool_count(&self) -> usize {
        self.tool_count
    }
}

/// A response rejected because it contains no tool proposals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolUsingAssistantResponseError {
    parts: Vec<AssistantResponsePart>,
}

impl ToolUsingAssistantResponseError {
    /// Returns the unchanged response parts.
    pub fn into_parts(self) -> Vec<AssistantResponsePart> {
        self.parts
    }
}
