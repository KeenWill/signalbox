//! Imported format projection dispatch for `docs/spec/conversation-import.md`.

use super::content::ImportedSourceMetadata;
use super::content::ImportedSpeaker;
use super::content::ImportedTranscriptContent;
use super::format::ImportedConversationFormat;
use super::structured_value::ImportedSourceAttestation;
use super::structured_value::ImportedStructuredValue;
use claude_code::project_claude_code_record;
use codex::project_codex_record;

mod claude_code;
mod codex;

#[derive(Eq, PartialEq)]
pub(super) struct ProjectedEntry {
    pub(super) source_speaker: ImportedSourceAttestation<ImportedSpeaker>,
    pub(super) content: ImportedTranscriptContent,
    pub(super) source: ImportedSourceMetadata,
}

#[derive(Clone, Copy)]
enum ClaudeCodeProjectionVersion {
    One,
    Two,
}

pub(super) fn projected_entries(
    format: ImportedConversationFormat,
    normalized: &ImportedStructuredValue,
) -> Result<Vec<ProjectedEntry>, ()> {
    match format {
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1 => {
            project_claude_code_record(normalized, ClaudeCodeProjectionVersion::One)
        }
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2 => {
            project_claude_code_record(normalized, ClaudeCodeProjectionVersion::Two)
        }
        ImportedConversationFormat::ClaudeCodeSessionJsonlV3 => {
            project_claude_code_record(normalized, ClaudeCodeProjectionVersion::Two)
        }
        ImportedConversationFormat::CodexRolloutJsonlV1
        | ImportedConversationFormat::CodexRolloutJsonlV2 => project_codex_record(normalized),
    }
}
