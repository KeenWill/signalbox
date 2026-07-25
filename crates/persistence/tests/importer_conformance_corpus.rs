#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this integration test uses fixed synthetic fixtures and explicit golden expectations"
)]

//! Synthetic conformance corpus for every imported-conversation format era.
//!
//! Claude Code version 1 is a stored-snapshot interpretation rather than the
//! active edge converter. Its fixture is normalized through the current parser
//! using only the vocabulary shared by versions 1 and 2, then reconstituted
//! under the domain's fixed version-1 projection. The remaining fixtures drive
//! their public edge converters directly.

use std::{fmt::Write, str};

use signalbox_application::ImportedConversationConverter;
use signalbox_conversation_import_claude_code::{
    ClaudeCodeJsonlConversionFailure, ClaudeCodeJsonlConverter,
};
use signalbox_conversation_import_codex::{
    CodexRolloutJsonlConversionFailure, CodexRolloutJsonlConverter,
};
use signalbox_domain::{
    ContextFrontierId, CreateSessionFromImportedFrontier, DirectModelSelection, DurableCommandId,
    ImportedConversation, ImportedConversationFormat, ImportedConversationId,
    ImportedSessionRelationship, ImportedSourceAttestation, ImportedText,
    ImportedTranscriptContent, ImportedTranscriptEntryId, ImportedTranscriptEntryInput,
    ModelSelectionRequest, SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionId,
    TranscriptAncestry,
};
use sqlx::types::Uuid;

const CLAUDE_V1_TOOL_ROUND: &[u8] =
    include_bytes!("fixtures/importer-conformance/claude-code-v1-tool-round.jsonl");
const CLAUDE_V2_BOUNDARY_LOSSES: &[u8] =
    include_bytes!("fixtures/importer-conformance/claude-code-v2-boundary-losses.jsonl");
const CODEX_V1_TOOL_ROUND: &[u8] =
    include_bytes!("fixtures/importer-conformance/codex-rollout-v1-tool-round.jsonl");
const CLAUDE_V2_DEPTH_128: &[u8] =
    include_bytes!("fixtures/importer-conformance/claude-code-v2-depth-128.jsonl");
const CLAUDE_V2_DEPTH_129: &[u8] =
    include_bytes!("fixtures/importer-conformance/claude-code-v2-depth-129.jsonl");
const CLAUDE_V2_UNDECODABLE_FRAGMENT: &[u8] =
    include_bytes!("fixtures/importer-conformance/claude-code-v2-undecodable-fragment.jsonl");
const CODEX_V1_TRUNCATED_FRAGMENT: &[u8] =
    include_bytes!("fixtures/importer-conformance/codex-rollout-v1-truncated-fragment.jsonl");

fn conversation_id(value: u128) -> ImportedConversationId {
    ImportedConversationId::from_uuid(Uuid::from_u128(value))
}

fn imported_entry_id(value: u128) -> ImportedTranscriptEntryId {
    ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(value))
}

fn session_id(value: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(value))
}

fn context_frontier_id(value: u128) -> ContextFrontierId {
    ContextFrontierId::from_uuid(Uuid::from_u128(value))
}

fn semantic_entry_id(value: u128) -> SemanticTranscriptEntryId {
    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value))
}

fn command_id(value: u128) -> DurableCommandId {
    DurableCommandId::from_uuid(Uuid::from_u128(value))
}

fn defaults(value: u128) -> SessionConfigurationDefaults {
    SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
        DirectModelSelection::from_uuid(Uuid::from_u128(value)),
    ))
}

#[track_caller]
fn convert_claude(source: &[u8]) -> ImportedConversation {
    let mut next_entry = 0x200_u128;
    ClaudeCodeJsonlConverter
        .convert(conversation_id(0x100), source, || {
            let identity = imported_entry_id(next_entry);
            next_entry = next_entry
                .checked_add(1)
                .expect("fixture identity range is bounded");
            identity
        })
        .unwrap_or_else(|error| {
            panic!(
                "synthetic Claude Code fixture should convert: {:?}",
                error.failure()
            )
        })
}

#[track_caller]
fn convert_codex(source: &[u8]) -> ImportedConversation {
    let mut next_entry = 0x400_u128;
    CodexRolloutJsonlConverter
        .convert(conversation_id(0x300), source, || {
            let identity = imported_entry_id(next_entry);
            next_entry = next_entry
                .checked_add(1)
                .expect("fixture identity range is bounded");
            identity
        })
        .unwrap_or_else(|error| {
            panic!(
                "synthetic Codex fixture should convert: {:?}",
                error.failure()
            )
        })
}

#[track_caller]
fn reconstitute_stored_claude_v1(source: &[u8]) -> ImportedConversation {
    let parsed = convert_claude(source);
    let entries = parsed
        .entries()
        .iter()
        .map(|entry| {
            ImportedTranscriptEntryInput::new(
                entry.identity(),
                entry.conversation(),
                entry.position(),
                entry.raw_record_position(),
                entry.record_entry_position(),
                entry.source_speaker().clone(),
                entry.content().clone(),
                entry.source().clone(),
            )
        })
        .collect();
    ImportedConversation::from_converted_records(
        parsed.id(),
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        parsed.raw_records().to_vec(),
        entries,
    )
    .expect("the legacy fixture uses the exact version-1 stored projection")
}

#[track_caller]
fn assert_every_entry_has_one_frontier(imported: &ImportedConversation) {
    assert_eq!(imported.frontiers().count(), imported.entries().len());
}

fn render_conversation(imported: &ImportedConversation) -> String {
    let mut rendered = format!(
        "format: {:?}\nraw_records: {}\n",
        imported.format(),
        imported.raw_records().len()
    );
    for (index, record) in imported.raw_records().iter().enumerate() {
        let raw = str::from_utf8(record.bytes()).expect("successful JSON fixture is UTF-8");
        writeln!(&mut rendered, "  {}: {raw:?}", index + 1)
            .expect("formatting into a string cannot fail");
    }
    writeln!(&mut rendered, "entries: {}", imported.entries().len())
        .expect("formatting into a string cannot fail");
    for entry in imported.entries() {
        writeln!(
            &mut rendered,
            "  {}: raw={}.{} speaker={:?}\n    content={:?}\n    source={:?}",
            entry.position().as_u64(),
            entry.raw_record_position().as_u64(),
            entry.record_entry_position().as_u64(),
            entry.source_speaker(),
            entry.content(),
            entry.source(),
        )
        .expect("formatting into a string cannot fail");
    }
    writeln!(
        &mut rendered,
        "addressable_frontiers: {}",
        imported.frontiers().count()
    )
    .expect("formatting into a string cannot fail");
    rendered
}

#[test]
fn s28_conformance_renderer_reports_each_raw_record_and_entry_boundary() {
    let imported = convert_claude(br#"{"type":"system"}"#);

    assert_eq!(
        render_conversation(&imported),
        concat!(
            "format: ClaudeCodeSessionJsonlV2\n",
            "raw_records: 1\n",
            "  1: \"{\\\"type\\\":\\\"system\\\"}\"\n",
            "entries: 1\n",
            "  1: raw=1.1 speaker=NotAttested\n",
            "    content=SourceEvent { source_type: Attested(ImportedText { utf8_len: 6, .. }) }\n",
            "    source=ImportedSourceMetadata { record_id: NotAttested, parent_record_id: NotAttested, source_session_id: NotAttested, timestamp: NotAttested, sidechain: NotAttested, metadata: NotAttested, message_role: NotAttested }\n",
            "addressable_frontiers: 1\n",
        )
    );
}

#[test]
fn s28_inv038_stored_claude_code_v1_tool_round_matches_golden() {
    let imported = reconstitute_stored_claude_v1(CLAUDE_V1_TOOL_ROUND);

    assert_eq!(
        imported.format(),
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1
    );
    assert_eq!(imported.raw_records().len(), 6);
    assert_eq!(imported.entries().len(), 7);
    assert!(matches!(
        imported.entries()[3].content(),
        ImportedTranscriptContent::ToolCall { .. }
    ));
    assert!(matches!(
        imported.entries()[4].content(),
        ImportedTranscriptContent::ToolResult { .. }
    ));
    assert!(matches!(
        imported.entries()[5].content(),
        ImportedTranscriptContent::SourceMessageBlock { .. }
    ));
    assert_every_entry_has_one_frontier(&imported);
    assert_eq!(
        render_conversation(&imported),
        include_str!("fixtures/importer-conformance/golden/claude-code-v1-tool-round.txt")
    );
}

#[test]
fn s28_inv038_claude_code_v2_boundary_losses_match_golden() {
    let imported = convert_claude(CLAUDE_V2_BOUNDARY_LOSSES);

    assert_eq!(
        imported.format(),
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2
    );
    assert_eq!(imported.raw_records().len(), 3);
    assert_eq!(imported.entries().len(), 4);
    assert_eq!(
        imported.entries()[0].source().parent_record_id(),
        &ImportedSourceAttestation::Attested(ImportedText::new(String::from(
            "missing-before-fixture"
        )))
    );
    assert!(matches!(
        imported.entries()[1].content(),
        ImportedTranscriptContent::SourceMessageBlock { .. }
    ));
    assert_eq!(
        imported.entries()[3].content(),
        &ImportedTranscriptContent::MessageContentAbsent(
            signalbox_domain::ImportedMessageContentAbsence::ContentAttestedAbsent
        )
    );
    assert_every_entry_has_one_frontier(&imported);
    assert_eq!(
        render_conversation(&imported),
        include_str!("fixtures/importer-conformance/golden/claude-code-v2-boundary-losses.txt")
    );
}

#[test]
fn s28_inv038_codex_rollout_v1_tool_round_matches_golden() {
    let imported = convert_codex(CODEX_V1_TOOL_ROUND);

    assert_eq!(
        imported.format(),
        ImportedConversationFormat::CodexRolloutJsonlV1
    );
    assert_eq!(imported.raw_records().len(), 7);
    assert_eq!(imported.entries().len(), 9);
    assert!(matches!(
        imported.entries()[5].content(),
        ImportedTranscriptContent::ToolCall { .. }
    ));
    assert!(matches!(
        imported.entries()[6].content(),
        ImportedTranscriptContent::ToolResult { .. }
    ));
    assert!(matches!(
        imported.entries()[7].content(),
        ImportedTranscriptContent::Text(_)
    ));
    assert_every_entry_has_one_frontier(&imported);
    assert_eq!(
        render_conversation(&imported),
        include_str!("fixtures/importer-conformance/golden/codex-rollout-v1-tool-round.txt")
    );
}

#[test]
fn s28_inv038_claude_code_v2_depth_128_matches_golden() {
    let imported = convert_claude(CLAUDE_V2_DEPTH_128);

    assert_eq!(imported.raw_records().len(), 1);
    assert_eq!(imported.entries().len(), 1);
    assert!(matches!(
        imported.entries()[0].content(),
        ImportedTranscriptContent::SourceEvent { .. }
    ));
    assert_every_entry_has_one_frontier(&imported);
    assert_eq!(
        render_conversation(&imported),
        include_str!("fixtures/importer-conformance/golden/claude-code-v2-depth-128.txt")
    );
}

#[test]
fn s28_inv038_claude_code_v2_depth_129_rejection_matches_golden() {
    let mut identity_calls = 0_u64;
    let error = ClaudeCodeJsonlConverter
        .convert(conversation_id(0x900), CLAUDE_V2_DEPTH_129, || {
            identity_calls += 1;
            imported_entry_id(0xa00)
        })
        .expect_err("the fixture exceeds the accepted container depth");
    let rendered = format!(
        "failure: {:?}\nentry_identity_calls: {identity_calls}\n",
        error.failure()
    );

    assert_eq!(
        error.failure(),
        ClaudeCodeJsonlConversionFailure::JsonDepthExceeded { line: 1 }
    );
    assert_eq!(identity_calls, 0);
    assert_eq!(
        rendered,
        include_str!("fixtures/importer-conformance/golden/claude-code-v2-depth-129-error.txt")
    );
}

#[test]
fn s28_inv038_claude_code_undecodable_fragment_rejection_matches_golden() {
    let mut identity_calls = 0_u64;
    let error = ClaudeCodeJsonlConverter
        .convert(
            conversation_id(0xb00),
            CLAUDE_V2_UNDECODABLE_FRAGMENT,
            || {
                identity_calls += 1;
                imported_entry_id(0xc00)
            },
        )
        .expect_err("a lone surrogate has no decoded Unicode scalar");
    let rendered = format!(
        "failure: {:?}\nentry_identity_calls: {identity_calls}\n",
        error.failure()
    );

    assert_eq!(
        error.failure(),
        ClaudeCodeJsonlConversionFailure::InvalidJson { line: 2 }
    );
    assert_eq!(identity_calls, 0);
    assert_eq!(
        rendered,
        include_str!(
            "fixtures/importer-conformance/golden/claude-code-v2-undecodable-fragment-error.txt"
        )
    );
}

#[test]
fn s28_inv038_codex_truncated_fragment_rejection_matches_golden() {
    let mut identity_calls = 0_u64;
    let error = CodexRolloutJsonlConverter
        .convert(conversation_id(0xd00), CODEX_V1_TRUNCATED_FRAGMENT, || {
            identity_calls += 1;
            imported_entry_id(0xe00)
        })
        .expect_err("the truncated final record is not valid JSON");
    let rendered = format!(
        "failure: {:?}\nentry_identity_calls: {identity_calls}\n",
        error.failure()
    );

    assert_eq!(
        error.failure(),
        CodexRolloutJsonlConversionFailure::InvalidJson { line: 2 }
    );
    assert_eq!(identity_calls, 0);
    assert_eq!(
        rendered,
        include_str!(
            "fixtures/importer-conformance/golden/codex-rollout-v1-truncated-fragment-error.txt"
        )
    );
}

#[test]
fn s28_inv038_inv039_import_only_resume_and_fork_match_golden() {
    let imported = convert_claude(CLAUDE_V2_BOUNDARY_LOSSES);
    let unchanged = imported.clone();
    let selected = imported
        .frontiers()
        .last()
        .expect("the fixture has an addressable final boundary");
    let mut next_resume_entry = 0x1100;
    let resume = CreateSessionFromImportedFrontier::new(
        command_id(0x1200),
        selected,
        ImportedSessionRelationship::Resume,
        defaults(0x1300),
    )
    .prepare(
        &imported,
        session_id(0x1400),
        context_frontier_id(0x1500),
        || {
            let identity = semantic_entry_id(next_resume_entry);
            next_resume_entry += 1;
            identity
        },
    )
    .expect("the selected imported prefix prepares for resume");
    let mut next_fork_entry = 0x1600;
    let fork = CreateSessionFromImportedFrontier::new(
        command_id(0x1700),
        selected,
        ImportedSessionRelationship::Fork,
        defaults(0x1800),
    )
    .prepare(
        &imported,
        session_id(0x1900),
        context_frontier_id(0x1a00),
        || {
            let identity = semantic_entry_id(next_fork_entry);
            next_fork_entry += 1;
            identity
        },
    )
    .expect("the selected imported prefix prepares for fork");
    let rendered = format!(
        "import_only:\n  format: {:?}\n  raw_records: {}\n  entries: {}\n  selected_through_position: {}\nadopt_resume:\n  relationship: {:?}\n  seed_entries: {}\n  imported_unchanged: {}\nadopt_fork:\n  relationship: {:?}\n  seed_entries: {}\n  imported_unchanged: {}\n",
        imported.format(),
        imported.raw_records().len(),
        imported.entries().len(),
        selected.through_position().as_u64(),
        resume.command().relationship(),
        resume.seed_snapshot().entry_count(),
        imported == unchanged,
        fork.command().relationship(),
        fork.seed_snapshot().entry_count(),
        imported == unchanged,
    );

    assert_eq!(
        resume.seed_snapshot().entry_count(),
        imported.entries().len()
    );
    assert_eq!(fork.seed_snapshot().entry_count(), imported.entries().len());
    assert_eq!(
        resume.session().provenance().ancestry(),
        TranscriptAncestry::ImportedConversation {
            source_frontier: selected,
            relationship: ImportedSessionRelationship::Resume,
        }
    );
    assert_eq!(
        fork.session().provenance().ancestry(),
        TranscriptAncestry::ImportedConversation {
            source_frontier: selected,
            relationship: ImportedSessionRelationship::Fork,
        }
    );
    assert_eq!(imported, unchanged);
    assert_eq!(
        rendered,
        include_str!("fixtures/importer-conformance/golden/adoption-modes.txt")
    );
}
