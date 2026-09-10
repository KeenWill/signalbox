//! Imported conversation source formats for `docs/spec/conversation-import.md`.

use std::hash::Hash;

/// One source format interpreted by one fixed Signalbox converter version.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ImportedConversationFormat {
    /// Claude Code session JSONL interpreted by converter version 1.
    ClaudeCodeSessionJsonlV1,
    /// Claude Code session JSONL interpreted by converter version 2.
    ClaudeCodeSessionJsonlV2,
    /// Claude Code session JSONL interpreted by resilient converter version 3.
    ClaudeCodeSessionJsonlV3,
    /// Codex rollout JSONL interpreted by converter version 1.
    CodexRolloutJsonlV1,
    /// Codex rollout JSONL interpreted by resilient converter version 2.
    CodexRolloutJsonlV2,
}

impl ImportedConversationFormat {
    pub(super) fn digest_tag(self) -> &'static [u8] {
        match self {
            Self::ClaudeCodeSessionJsonlV1 => b"claude-code-session-jsonl-v1",
            Self::ClaudeCodeSessionJsonlV2 => b"claude-code-session-jsonl-v2",
            Self::ClaudeCodeSessionJsonlV3 => b"claude-code-session-jsonl-v3",
            Self::CodexRolloutJsonlV1 => b"codex-rollout-jsonl-v1",
            Self::CodexRolloutJsonlV2 => b"codex-rollout-jsonl-v2",
        }
    }
}
