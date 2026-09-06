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

use std::{fmt::Write, fs, path::Path, str};

use expect_test::expect_file;
use signalbox_application::ImportedConversationConverter;
use signalbox_conversation_import_claude_code::{
    ClaudeCodeJsonlConversionFailure, ClaudeCodeJsonlConverter,
};
use signalbox_conversation_import_codex::{
    CodexRolloutJsonlConversionFailure, CodexRolloutJsonlConverter,
};
use signalbox_domain::{
    ContextFrontierId, CreateSessionFromImportedFrontier, DirectModelSelection, DurableCommandId,
    ImportedConversation, ImportedConversationFormat, ImportedConversationId, ImportedMediaSource,
    ImportedSessionRelationship, ImportedSourceAttestation, ImportedSourceMetadata,
    ImportedSpeaker, ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
    ImportedToolResultBlock, ImportedToolResultValue, ImportedTranscriptContent,
    ImportedTranscriptEntry, ImportedTranscriptEntryId, ImportedTranscriptEntryInput,
    ModelSelectionRequest, SemanticTranscriptEntryId, SemanticTranscriptEntryPayload,
    SessionConfigurationDefaults, SessionId, TranscriptAncestry,
};
use sqlx::types::Uuid;

/// Absolute path to one golden under this crate's fixture directory.
///
/// A relative `expect_file!` path is joined onto a workspace root expect-test
/// derives at run time, which follows Cargo's configuration discovery and so
/// depends on the directory the run was launched from rather than on the tree
/// that was compiled. `CARGO_MANIFEST_DIR` is substituted at compile time by
/// the checkout doing the compiling, so an absolute path built from it names
/// that checkout's goldens under every launch, and expect-test takes an
/// absolute path verbatim for both the comparison and the `UPDATE_EXPECT=1`
/// rewrite.
macro_rules! golden {
    ($name:literal) => {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/importer-conformance/golden/",
            $name
        )
    };
}

const CLAUDE_V1_TOOL_ROUND: &[u8] =
    include_bytes!("fixtures/importer-conformance/claude-code-v1-tool-round.jsonl");
const CLAUDE_V2_BOUNDARY_LOSSES: &[u8] =
    include_bytes!("fixtures/importer-conformance/claude-code-v2-boundary-losses.jsonl");
const CODEX_V1_TOOL_ROUND: &[u8] =
    include_bytes!("fixtures/importer-conformance/codex-rollout-v1-tool-round.jsonl");
const CODEX_V1_STRUCTURED_TOOLS: &[u8] =
    include_bytes!("fixtures/importer-conformance/codex-rollout-v1-structured-tools.jsonl");
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

fn render_attestation<Value>(
    attestation: &ImportedSourceAttestation<Value>,
    render_value: impl FnOnce(&Value) -> String,
) -> String {
    match attestation {
        ImportedSourceAttestation::Attested(value) => {
            format!("attested({})", render_value(value))
        }
        ImportedSourceAttestation::AttestedAbsent => String::from("attested_absent"),
        ImportedSourceAttestation::NotAttested => String::from("not_attested"),
    }
}

fn render_text_attestation(attestation: &ImportedSourceAttestation<ImportedText>) -> String {
    render_attestation(attestation, |value| format!("{:?}", value.as_str()))
}

fn render_bool_attestation(attestation: &ImportedSourceAttestation<bool>) -> String {
    render_attestation(attestation, bool::to_string)
}

fn render_speaker_attestation(attestation: &ImportedSourceAttestation<ImportedSpeaker>) -> String {
    render_attestation(attestation, |speaker| format!("{speaker:?}"))
}

fn render_structured(value: &ImportedStructuredValue) -> String {
    match value {
        ImportedStructuredValue::Null => String::from("null"),
        ImportedStructuredValue::Boolean(value) => value.to_string(),
        ImportedStructuredValue::Number(value) => String::from(value.as_str()),
        ImportedStructuredValue::String(value) => format!("{:?}", value.as_str()),
        ImportedStructuredValue::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(render_structured)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ImportedStructuredValue::Object(members) => format!(
            "{{{}}}",
            members
                .iter()
                .map(|member| format!(
                    "{:?}: {}",
                    member.name().as_str(),
                    render_structured(member.value())
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn render_structured_attestation(
    attestation: &ImportedSourceAttestation<ImportedStructuredValue>,
) -> String {
    render_attestation(attestation, render_structured)
}

#[track_caller]
fn fixture_text_member<'a>(
    value: &'a ImportedStructuredValue,
    member_name: &str,
) -> &'a ImportedText {
    let ImportedStructuredValue::Object(members) = value else {
        panic!("fixture value containing {member_name:?} must be an object");
    };
    let member = members
        .iter()
        .find(|member| member.name().as_str() == member_name)
        .unwrap_or_else(|| panic!("fixture object must contain {member_name:?}"));
    let ImportedStructuredValue::String(value) = member.value() else {
        panic!("fixture member {member_name:?} must contain text");
    };
    value
}

#[track_caller]
fn fixture_object_member<'a>(
    value: &'a ImportedStructuredValue,
    member_name: &str,
) -> &'a ImportedStructuredValue {
    let ImportedStructuredValue::Object(members) = value else {
        panic!("fixture value containing {member_name:?} must be an object");
    };
    let member = members
        .iter()
        .find(|member| member.name().as_str() == member_name)
        .unwrap_or_else(|| panic!("fixture object must contain {member_name:?}"));
    let value @ ImportedStructuredValue::Object(_) = member.value() else {
        panic!("fixture member {member_name:?} must contain an object");
    };
    value
}

#[track_caller]
fn fixture_array_member<'a>(
    value: &'a ImportedStructuredValue,
    member_name: &str,
) -> &'a [ImportedStructuredValue] {
    let ImportedStructuredValue::Object(members) = value else {
        panic!("fixture value containing {member_name:?} must be an object");
    };
    let member = members
        .iter()
        .find(|member| member.name().as_str() == member_name)
        .unwrap_or_else(|| panic!("fixture object must contain {member_name:?}"));
    let ImportedStructuredValue::Array(elements) = member.value() else {
        panic!("fixture member {member_name:?} must contain an array");
    };
    elements
}

fn render_media(media: &ImportedMediaSource) -> String {
    format!(
        "kind={} media_type={} data={}",
        render_text_attestation(media.kind()),
        render_text_attestation(media.media_type()),
        render_text_attestation(media.data())
    )
}

fn render_media_attestation(
    attestation: &ImportedSourceAttestation<ImportedMediaSource>,
) -> String {
    render_attestation(attestation, render_media)
}

fn render_result_block(block: &ImportedToolResultBlock) -> String {
    match block {
        ImportedToolResultBlock::Text(text) => {
            format!("text({})", render_text_attestation(text))
        }
        ImportedToolResultBlock::Image(source) => {
            format!("image({})", render_media_attestation(source))
        }
        ImportedToolResultBlock::ToolReference { tool_name } => {
            format!(
                "tool_reference(name={})",
                render_text_attestation(tool_name)
            )
        }
        ImportedToolResultBlock::SourceResultBlock { source_type } => {
            format!(
                "source_result_block(type={})",
                render_text_attestation(source_type)
            )
        }
    }
}

fn render_tool_result_value(value: &ImportedToolResultValue) -> String {
    match value {
        ImportedToolResultValue::Text(text) => format!("text({:?})", text.as_str()),
        ImportedToolResultValue::Blocks(blocks) => format!(
            "blocks([{}])",
            blocks
                .iter()
                .map(render_result_block)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn render_content(content: &ImportedTranscriptContent) -> String {
    match content {
        ImportedTranscriptContent::SourceEvent { source_type } => {
            format!(
                "source_event(type={})",
                render_text_attestation(source_type)
            )
        }
        ImportedTranscriptContent::SourceMessageBlock { source_type } => {
            format!(
                "source_message_block(type={})",
                render_text_attestation(source_type)
            )
        }
        ImportedTranscriptContent::Text(text) => {
            format!("text({})", render_text_attestation(text))
        }
        ImportedTranscriptContent::ToolCall {
            source_call_id,
            name,
            input,
            caller,
        } => format!(
            "tool_call(id={} name={} input={} caller={})",
            render_text_attestation(source_call_id),
            render_text_attestation(name),
            render_structured_attestation(input),
            render_structured_attestation(caller)
        ),
        ImportedTranscriptContent::ToolResult {
            source_call_id,
            content,
            is_error,
        } => format!(
            "tool_result(id={} content={} is_error={})",
            render_text_attestation(source_call_id),
            render_attestation(content, render_tool_result_value),
            render_bool_attestation(is_error)
        ),
        ImportedTranscriptContent::Thinking {
            thinking,
            signature,
        } => format!(
            "thinking(text={} signature={})",
            render_text_attestation(thinking),
            render_text_attestation(signature)
        ),
        ImportedTranscriptContent::RedactedThinking { data } => {
            format!("redacted_thinking(data={})", render_text_attestation(data))
        }
        ImportedTranscriptContent::Document { source } => {
            format!("document(source={})", render_media_attestation(source))
        }
        ImportedTranscriptContent::MessageContentAbsent(absence) => {
            format!("message_content_absent({absence:?})")
        }
    }
}

fn render_source(source: &ImportedSourceMetadata) -> String {
    format!(
        "record_id={} parent_record_id={} source_session_id={} timestamp={} sidechain={} metadata={} message_role={}",
        render_text_attestation(source.record_id()),
        render_text_attestation(source.parent_record_id()),
        render_text_attestation(source.source_session_id()),
        render_text_attestation(source.timestamp()),
        render_bool_attestation(source.sidechain()),
        render_bool_attestation(source.metadata()),
        render_speaker_attestation(source.message_role())
    )
}

fn render_conversation(imported: &ImportedConversation) -> String {
    let mut rendered = format!(
        "format: {:?}\nraw_records: {}\n",
        imported.format(),
        imported.raw_records().len()
    );
    for (index, record) in imported.raw_records().iter().enumerate() {
        let raw = str::from_utf8(record.bytes()).expect("successful JSON fixture is UTF-8");
        writeln!(
            &mut rendered,
            "  {}: raw={raw:?}\n    normalized={}",
            index + 1,
            render_structured(record.normalized())
        )
        .expect("formatting into a string cannot fail");
    }
    writeln!(&mut rendered, "entries: {}", imported.entries().len())
        .expect("formatting into a string cannot fail");
    for entry in imported.entries() {
        writeln!(
            &mut rendered,
            "  {}: raw={}.{} speaker={}\n    content={}\n    source={}",
            entry.position().as_u64(),
            entry.raw_record_position().as_u64(),
            entry.record_entry_position().as_u64(),
            render_speaker_attestation(entry.source_speaker()),
            render_content(entry.content()),
            render_source(entry.source()),
        )
        .expect("formatting into a string cannot fail");
    }
    let frontiers = imported.frontiers().collect::<Vec<_>>();
    writeln!(&mut rendered, "addressable_frontiers: {}", frontiers.len())
        .expect("formatting into a string cannot fail");
    for (index, frontier) in frontiers.iter().enumerate() {
        writeln!(
            &mut rendered,
            "  {}: conversation={} through_entry={} through_position={}",
            index + 1,
            frontier.conversation().as_uuid(),
            frontier.through_entry().as_uuid(),
            frontier.through_position().as_u64(),
        )
        .expect("formatting into a string cannot fail");
    }
    rendered
}

#[test]
fn s28_fixture_text_member_returns_the_named_fixture_value() {
    let expected = ImportedText::new(String::from("selected-value"));
    let fixture = ImportedStructuredValue::Object(
        vec![
            ImportedStructuredObjectMember::new(
                ImportedText::new(String::from("other")),
                ImportedStructuredValue::String(ImportedText::new(String::from("other-value"))),
            ),
            ImportedStructuredObjectMember::new(
                ImportedText::new(String::from("selected")),
                ImportedStructuredValue::String(expected.clone()),
            ),
        ]
        .into_boxed_slice(),
    );

    assert_eq!(fixture_text_member(&fixture, "selected"), &expected);
}

#[test]
fn s28_fixture_object_member_returns_the_named_nested_object() {
    let expected = ImportedStructuredValue::Object(
        vec![ImportedStructuredObjectMember::new(
            ImportedText::new(String::from("id")),
            ImportedStructuredValue::String(ImportedText::new(String::from("nested-value"))),
        )]
        .into_boxed_slice(),
    );
    let fixture = ImportedStructuredValue::Object(
        vec![
            ImportedStructuredObjectMember::new(
                ImportedText::new(String::from("other")),
                ImportedStructuredValue::String(ImportedText::new(String::from("other-value"))),
            ),
            ImportedStructuredObjectMember::new(
                ImportedText::new(String::from("payload")),
                expected.clone(),
            ),
        ]
        .into_boxed_slice(),
    );

    assert_eq!(fixture_object_member(&fixture, "payload"), &expected);
}

#[test]
fn s28_fixture_array_member_returns_the_named_array_elements() {
    let expected = vec![
        ImportedStructuredValue::String(ImportedText::new(String::from("first"))),
        ImportedStructuredValue::Null,
    ];
    let fixture = ImportedStructuredValue::Object(
        vec![
            ImportedStructuredObjectMember::new(
                ImportedText::new(String::from("other")),
                ImportedStructuredValue::String(ImportedText::new(String::from("other-value"))),
            ),
            ImportedStructuredObjectMember::new(
                ImportedText::new(String::from("elements")),
                ImportedStructuredValue::Array(expected.clone().into_boxed_slice()),
            ),
        ]
        .into_boxed_slice(),
    );

    assert_eq!(fixture_array_member(&fixture, "elements"), expected);
}

#[test]
fn s28_conformance_renderer_reports_each_raw_record_and_entry_boundary() {
    let imported = convert_claude(br#"{"type":"system"}"#);

    assert_eq!(
        render_conversation(&imported),
        concat!(
            "format: ClaudeCodeSessionJsonlV2\n",
            "raw_records: 1\n",
            "  1: raw=\"{\\\"type\\\":\\\"system\\\"}\"\n",
            "    normalized={\"type\": \"system\"}\n",
            "entries: 1\n",
            "  1: raw=1.1 speaker=not_attested\n",
            "    content=source_event(type=attested(\"system\"))\n",
            "    source=record_id=not_attested parent_record_id=not_attested source_session_id=not_attested timestamp=not_attested sidechain=not_attested metadata=not_attested message_role=not_attested\n",
            "addressable_frontiers: 1\n",
            "  1: conversation=00000000-0000-0000-0000-000000000100 through_entry=00000000-0000-0000-0000-000000000200 through_position=1\n",
        )
    );
}

#[test]
fn s28_stored_claude_code_v1_tool_round_matches_golden() {
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
    expect_file![golden!("claude-code-v1-tool-round.txt")]
        .assert_eq(&render_conversation(&imported));
}

#[test]
fn s28_claude_code_v2_boundary_losses_match_golden() {
    let imported = convert_claude(CLAUDE_V2_BOUNDARY_LOSSES);

    assert_eq!(
        imported.format(),
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2
    );
    assert_eq!(imported.raw_records().len(), 3);
    assert_eq!(imported.entries().len(), 4);
    let fixture_parent = fixture_text_member(imported.raw_records()[0].normalized(), "parentUuid");
    assert_eq!(
        imported.entries()[0].source().parent_record_id(),
        &ImportedSourceAttestation::Attested(fixture_parent.clone())
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
    expect_file![golden!("claude-code-v2-boundary-losses.txt")]
        .assert_eq(&render_conversation(&imported));
}

#[test]
fn s28_codex_rollout_v1_tool_round_matches_golden() {
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
    expect_file![golden!("codex-rollout-v1-tool-round.txt")]
        .assert_eq(&render_conversation(&imported));
}

/// Checks that one converted entry occupies the stated imported position and
/// carries exactly the stated content. Both facts stay at the call site: the
/// entry index states where the converter placed the payload, the position
/// states the ordinal it stamped on it, and the content states the whole
/// mapping — so dropping, reordering, or altering any mapped field fails here
/// rather than only in the golden.
#[track_caller]
fn assert_entry_maps(
    entry: &ImportedTranscriptEntry,
    position: u64,
    expected: ImportedTranscriptContent,
) {
    assert_eq!(entry.position().as_u64(), position);
    assert_eq!(entry.content(), &expected);
}

/// S28: the Codex era's structured tool vocabulary — tool search
/// call and output, local shell call, web search call, and the custom tool call
/// and its output — retains each call identity, structured argument or action
/// value, and result content in converted order. The targeted asserts below
/// carry that enforcement (testing-style rule 10); the golden supplements them
/// with the full raw-record, source-metadata, and frontier shape.
#[test]
fn s28_codex_rollout_v1_structured_tools_match_golden() {
    let imported = convert_codex(CODEX_V1_STRUCTURED_TOOLS);
    let session_meta = imported.raw_records()[0].normalized();
    let search_call = fixture_object_member(imported.raw_records()[1].normalized(), "payload");
    let search_output = fixture_object_member(imported.raw_records()[2].normalized(), "payload");
    let shell_call = fixture_object_member(imported.raw_records()[3].normalized(), "payload");
    let web_call = fixture_object_member(imported.raw_records()[4].normalized(), "payload");
    let custom_call = fixture_object_member(imported.raw_records()[5].normalized(), "payload");
    let custom_output = fixture_object_member(imported.raw_records()[6].normalized(), "payload");

    assert_eq!(imported.raw_records().len(), 7);
    assert_eq!(imported.entries().len(), 7);
    assert_entry_maps(
        &imported.entries()[0],
        1,
        ImportedTranscriptContent::SourceEvent {
            source_type: ImportedSourceAttestation::Attested(
                fixture_text_member(session_meta, "type").clone(),
            ),
        },
    );
    assert_entry_maps(
        &imported.entries()[1],
        2,
        ImportedTranscriptContent::ToolCall {
            source_call_id: ImportedSourceAttestation::Attested(
                fixture_text_member(search_call, "call_id").clone(),
            ),
            name: ImportedSourceAttestation::NotAttested,
            input: ImportedSourceAttestation::Attested(
                fixture_object_member(search_call, "arguments").clone(),
            ),
            caller: ImportedSourceAttestation::NotAttested,
        },
    );
    assert_entry_maps(
        &imported.entries()[2],
        3,
        ImportedTranscriptContent::ToolResult {
            source_call_id: ImportedSourceAttestation::Attested(
                fixture_text_member(search_output, "call_id").clone(),
            ),
            content: ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                vec![
                    ImportedToolResultBlock::SourceResultBlock {
                        source_type: ImportedSourceAttestation::Attested(
                            fixture_text_member(
                                &fixture_array_member(search_output, "tools")[0],
                                "type",
                            )
                            .clone(),
                        ),
                    },
                    ImportedToolResultBlock::SourceResultBlock {
                        source_type: ImportedSourceAttestation::NotAttested,
                    },
                ]
                .into_boxed_slice(),
            )),
            is_error: ImportedSourceAttestation::NotAttested,
        },
    );
    assert_entry_maps(
        &imported.entries()[3],
        4,
        ImportedTranscriptContent::ToolCall {
            source_call_id: ImportedSourceAttestation::Attested(
                fixture_text_member(shell_call, "call_id").clone(),
            ),
            name: ImportedSourceAttestation::NotAttested,
            input: ImportedSourceAttestation::Attested(
                fixture_object_member(shell_call, "action").clone(),
            ),
            caller: ImportedSourceAttestation::NotAttested,
        },
    );
    assert_entry_maps(
        &imported.entries()[4],
        5,
        ImportedTranscriptContent::ToolCall {
            source_call_id: ImportedSourceAttestation::Attested(
                fixture_text_member(web_call, "id").clone(),
            ),
            name: ImportedSourceAttestation::NotAttested,
            input: ImportedSourceAttestation::Attested(
                fixture_object_member(web_call, "action").clone(),
            ),
            caller: ImportedSourceAttestation::NotAttested,
        },
    );
    assert_entry_maps(
        &imported.entries()[5],
        6,
        ImportedTranscriptContent::ToolCall {
            source_call_id: ImportedSourceAttestation::Attested(
                fixture_text_member(custom_call, "call_id").clone(),
            ),
            name: ImportedSourceAttestation::Attested(
                fixture_text_member(custom_call, "name").clone(),
            ),
            input: ImportedSourceAttestation::Attested(ImportedStructuredValue::String(
                fixture_text_member(custom_call, "input").clone(),
            )),
            caller: ImportedSourceAttestation::NotAttested,
        },
    );
    assert_entry_maps(
        &imported.entries()[6],
        7,
        ImportedTranscriptContent::ToolResult {
            source_call_id: ImportedSourceAttestation::Attested(
                fixture_text_member(custom_output, "call_id").clone(),
            ),
            content: ImportedSourceAttestation::Attested(ImportedToolResultValue::Text(
                fixture_text_member(custom_output, "output").clone(),
            )),
            is_error: ImportedSourceAttestation::NotAttested,
        },
    );
    expect_file![golden!("codex-rollout-v1-structured-tools.txt")]
        .assert_eq(&render_conversation(&imported));
}

#[test]
fn s28_claude_code_v2_depth_128_matches_golden() {
    let imported = convert_claude(CLAUDE_V2_DEPTH_128);

    assert_eq!(imported.raw_records().len(), 1);
    assert_eq!(imported.entries().len(), 1);
    assert!(matches!(
        imported.entries()[0].content(),
        ImportedTranscriptContent::SourceEvent { .. }
    ));
    expect_file![golden!("claude-code-v2-depth-128.txt")]
        .assert_eq(&render_conversation(&imported));
}

#[test]
fn s28_claude_code_v2_depth_129_rejection_matches_golden() {
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
    expect_file![golden!("claude-code-v2-depth-129-error.txt")].assert_eq(&rendered);
}

#[test]
fn s28_claude_code_undecodable_fragment_rejection_matches_golden() {
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
    expect_file![golden!("claude-code-v2-undecodable-fragment-error.txt")].assert_eq(&rendered);
}

#[test]
fn s28_codex_truncated_fragment_rejection_matches_golden() {
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
    expect_file![golden!("codex-rollout-v1-truncated-fragment-error.txt")].assert_eq(&rendered);
}

#[test]
fn s28_import_only_resume_and_fork_match_golden() {
    let imported = convert_claude(CLAUDE_V2_BOUNDARY_LOSSES);
    let selected = imported
        .frontiers()
        .nth(1)
        .expect("the fixture has an interior addressable boundary");
    let expected_seed_entries = imported
        .entries()
        .get(..2)
        .expect("the fixture has the two entries preceding the selected boundary");
    let expected_first_payload = SemanticTranscriptEntryPayload::Imported {
        imported_entry: expected_seed_entries[0].identity(),
        source_speaker: expected_seed_entries[0].source_speaker().clone(),
        content: expected_seed_entries[0].content().clone(),
    };
    let expected_second_payload = SemanticTranscriptEntryPayload::Imported {
        imported_entry: expected_seed_entries[1].identity(),
        source_speaker: expected_seed_entries[1].source_speaker().clone(),
        content: expected_seed_entries[1].content().clone(),
    };
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
        "import_only:\n  format: {:?}\n  raw_records: {}\n  entries: {}\n  selected_through_position: {}\nadopt_resume:\n  relationship: {:?}\n  seed_entries: {}\nadopt_fork:\n  relationship: {:?}\n  seed_entries: {}\n",
        imported.format(),
        imported.raw_records().len(),
        imported.entries().len(),
        selected.through_position().as_u64(),
        resume.command().relationship(),
        resume.seed_snapshot().entry_count(),
        fork.command().relationship(),
        fork.seed_snapshot().entry_count(),
    );

    assert_eq!(resume.seed_snapshot().entry_count(), 2);
    assert_eq!(fork.seed_snapshot().entry_count(), 2);
    assert_eq!(resume.semantic_entries().len(), expected_seed_entries.len());
    assert_eq!(fork.semantic_entries().len(), expected_seed_entries.len());
    assert_eq!(
        resume.semantic_entries()[0].payload(),
        &expected_first_payload
    );
    assert_eq!(
        resume.semantic_entries()[1].payload(),
        &expected_second_payload
    );
    assert_eq!(
        fork.semantic_entries()[0].payload(),
        &expected_first_payload
    );
    assert_eq!(
        fork.semantic_entries()[1].payload(),
        &expected_second_payload
    );
    assert_eq!(
        resume.seed_snapshot().ordered_entries().collect::<Vec<_>>(),
        vec![
            resume.semantic_entries()[0].reference(),
            resume.semantic_entries()[1].reference(),
        ]
    );
    assert_eq!(
        fork.seed_snapshot().ordered_entries().collect::<Vec<_>>(),
        vec![
            fork.semantic_entries()[0].reference(),
            fork.semantic_entries()[1].reference(),
        ]
    );
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
    expect_file![golden!("adoption-modes.txt")].assert_eq(&rendered);
}

/// Root of the checkout this test was compiled from.
///
/// `CARGO_MANIFEST_DIR` is `<root>/crates/persistence`, so the root is two
/// levels above it. Taking it from the compile-time value rather than the
/// process environment keeps it independent of where a run was launched.
fn checkout_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("this crate sits two levels below the checkout root")
}

/// Guards the pin that keeps `UPDATE_EXPECT=1` rewriting inline `expect![...]`
/// literals in the checkout being compiled.
///
/// The goldens above do not depend on it: [`golden!`] hands expect-test an
/// absolute path. Inline literals cannot be anchored that way — expect-test
/// locates the source file to rewrite by joining `file!()` onto a workspace
/// root it derives at run time, from `CARGO_WORKSPACE_DIR` or, without it, the
/// topmost ancestor of `CARGO_MANIFEST_DIR` holding a `Cargo.toml`. That
/// ancestor walk leaves a linked worktree whose path lies inside another
/// checkout, and rewrites land in the outer tree's sources. Every crate using
/// inline expectations depends on `.cargo/config.toml` pinning the variable
/// per checkout, so this asserts the checked-in pin itself: Cargo loads that
/// file from the working directory's ancestors rather than the manifest's, so
/// a guard watching the loaded value would report how a run was launched
/// instead of whether the mechanism is still there.
#[test]
fn s28_inline_expectations_are_pinned_to_the_compiled_checkout() {
    let configuration = fs::read_to_string(checkout_root().join(".cargo/config.toml"))
        .expect("the checkout's Cargo configuration is committed");
    let configuration: toml::Table = configuration
        .parse()
        .expect("the checkout's Cargo configuration is TOML");

    let pin = &configuration["env"]["CARGO_WORKSPACE_DIR"];

    assert_eq!(pin["value"].as_str(), Some(""));
    assert_eq!(pin["relative"].as_bool(), Some(true));
    assert_eq!(pin["force"].as_bool(), Some(true));
}
