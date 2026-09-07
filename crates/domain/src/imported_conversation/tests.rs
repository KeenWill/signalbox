//! Imported conversation tests for `docs/spec/conversation-import.md`.

use super::projection::projected_entries;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use super::{
    ImportedConversation, ImportedConversationDisplayTitle, ImportedConversationDisplayTitleError,
    ImportedConversationFormat, ImportedConversationReconstitutionFailure,
    ImportedConversationReconstitutionInput, ImportedConversationSourceDigest, ImportedJsonNumber,
    ImportedMessageContentAbsence, ImportedRawRecordConversionDigest, ImportedRawRecordHash,
    ImportedRawRecordPosition, ImportedRawSourceRecord, ImportedRawSourceRecordReconstitutionInput,
    ImportedRecordEntryPosition, ImportedSourceAttestation, ImportedSourceMetadata,
    ImportedSpeaker, ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
    ImportedToolResultBlock, ImportedToolResultValue, ImportedTranscriptContent,
    ImportedTranscriptEntryInput, ImportedTranscriptPosition,
};
use crate::{ImportedConversationId, ImportedTranscriptEntryId};
use uuid::Uuid;

fn conversation(value: u128) -> ImportedConversationId {
    ImportedConversationId::from_uuid(Uuid::from_u128(value))
}

fn entry(value: u128) -> ImportedTranscriptEntryId {
    ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(value))
}

fn text(value: &str) -> ImportedText {
    ImportedText::new(String::from(value))
}

fn object(member: (&str, ImportedStructuredValue)) -> ImportedStructuredValue {
    object_with_members(vec![member])
}

fn object_with_members(members: Vec<(&str, ImportedStructuredValue)>) -> ImportedStructuredValue {
    ImportedStructuredValue::Object(
        members
            .into_iter()
            .map(|(name, value)| ImportedStructuredObjectMember::new(text(name), value))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    )
}

fn message_record(speaker: &str, content: ImportedStructuredValue) -> ImportedStructuredValue {
    object_with_members(vec![
        ("type", ImportedStructuredValue::String(text(speaker))),
        ("message", object(("content", content))),
    ])
}

fn nested_array(container_count: usize) -> ImportedStructuredValue {
    let mut value = ImportedStructuredValue::Null;
    for _ in 0..container_count {
        value = ImportedStructuredValue::Array(vec![value].into_boxed_slice());
    }
    value
}

#[test]
fn codex_reprojection_rejects_non_string_named_tool_input() {
    let normalized = object_with_members(vec![
        (
            "type",
            ImportedStructuredValue::String(text("response_item")),
        ),
        (
            "payload",
            object_with_members(vec![
                (
                    "type",
                    ImportedStructuredValue::String(text("function_call")),
                ),
                ("arguments", object(("key", ImportedStructuredValue::Null))),
            ]),
        ),
    ]);

    assert!(
        projected_entries(ImportedConversationFormat::CodexRolloutJsonlV1, &normalized).is_err()
    );
}

/// Reprojects one `response_item` payload through the Codex projection and
/// checks that it yields exactly the expected single entry, whose speaker
/// the projection leaves unattested rather than fabricating one. The
/// speaker expectation is fixed here — never a per-call value — because no
/// Codex tool payload attests a speaker; the helper's name carries it to
/// every call site.
#[track_caller]
fn assert_codex_payload_projects_one_entry_attesting_no_speaker(
    payload: ImportedStructuredValue,
    expected: ImportedTranscriptContent,
) {
    let normalized = object_with_members(vec![
        (
            "type",
            ImportedStructuredValue::String(text("response_item")),
        ),
        ("payload", payload),
    ]);

    let projected = projected_entries(ImportedConversationFormat::CodexRolloutJsonlV1, &normalized)
        .expect("a recognized Codex tool payload reprojects");

    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].content, expected);
    assert_eq!(
        projected[0].source_speaker,
        ImportedSourceAttestation::NotAttested
    );
}

/// the Codex reprojection maps a `tool_search_call`'s exact `arguments` value as tool input and
/// fabricates no tool name for it.
#[test]
fn codex_reprojection_maps_tool_search_call_arguments_without_a_name() {
    assert_codex_payload_projects_one_entry_attesting_no_speaker(
        object_with_members(vec![
            (
                "type",
                ImportedStructuredValue::String(text("tool_search_call")),
            ),
            (
                "call_id",
                ImportedStructuredValue::String(text("call-search")),
            ),
            (
                "arguments",
                object(("query", ImportedStructuredValue::String(text("read_file")))),
            ),
        ]),
        ImportedTranscriptContent::ToolCall {
            source_call_id: ImportedSourceAttestation::Attested(text("call-search")),
            name: ImportedSourceAttestation::NotAttested,
            input: ImportedSourceAttestation::Attested(object((
                "query",
                ImportedStructuredValue::String(text("read_file")),
            ))),
            caller: ImportedSourceAttestation::NotAttested,
        },
    );
}

/// the Codex reprojection maps a `local_shell_call`'s exact `action` value as tool input and
/// fabricates no tool name for it.
#[test]
fn codex_reprojection_maps_local_shell_call_action_without_a_name() {
    assert_codex_payload_projects_one_entry_attesting_no_speaker(
        object_with_members(vec![
            (
                "type",
                ImportedStructuredValue::String(text("local_shell_call")),
            ),
            (
                "call_id",
                ImportedStructuredValue::String(text("call-shell")),
            ),
            (
                "action",
                object(("command", ImportedStructuredValue::String(text("list")))),
            ),
        ]),
        ImportedTranscriptContent::ToolCall {
            source_call_id: ImportedSourceAttestation::Attested(text("call-shell")),
            name: ImportedSourceAttestation::NotAttested,
            input: ImportedSourceAttestation::Attested(object((
                "command",
                ImportedStructuredValue::String(text("list")),
            ))),
            caller: ImportedSourceAttestation::NotAttested,
        },
    );
}

/// the Codex reprojection takes a web-search call's identity from the item `id`; the payload also
/// states a competing `call_id` the mapping must not read.
#[test]
fn codex_reprojection_maps_web_search_item_id_as_call_identity() {
    assert_codex_payload_projects_one_entry_attesting_no_speaker(
        object_with_members(vec![
            (
                "type",
                ImportedStructuredValue::String(text("web_search_call")),
            ),
            ("id", ImportedStructuredValue::String(text("item-web"))),
            (
                "call_id",
                ImportedStructuredValue::String(text("unread-call-id")),
            ),
            (
                "action",
                object(("query", ImportedStructuredValue::String(text("catalog")))),
            ),
        ]),
        ImportedTranscriptContent::ToolCall {
            source_call_id: ImportedSourceAttestation::Attested(text("item-web")),
            name: ImportedSourceAttestation::NotAttested,
            input: ImportedSourceAttestation::Attested(object((
                "query",
                ImportedStructuredValue::String(text("catalog")),
            ))),
            caller: ImportedSourceAttestation::NotAttested,
        },
    );
}

/// the Codex reprojection reads a custom tool call's payload from `input` while retaining its exact
/// attested name.
#[test]
fn codex_reprojection_maps_custom_tool_call_input_field() {
    assert_codex_payload_projects_one_entry_attesting_no_speaker(
        object_with_members(vec![
            (
                "type",
                ImportedStructuredValue::String(text("custom_tool_call")),
            ),
            (
                "call_id",
                ImportedStructuredValue::String(text("call-custom")),
            ),
            ("name", ImportedStructuredValue::String(text("apply_patch"))),
            (
                "input",
                ImportedStructuredValue::String(text("*** Begin Patch")),
            ),
        ]),
        ImportedTranscriptContent::ToolCall {
            source_call_id: ImportedSourceAttestation::Attested(text("call-custom")),
            name: ImportedSourceAttestation::Attested(text("apply_patch")),
            input: ImportedSourceAttestation::Attested(ImportedStructuredValue::String(text(
                "*** Begin Patch",
            ))),
            caller: ImportedSourceAttestation::NotAttested,
        },
    );
}

/// the Codex reprojection maps a `custom_tool_call_output`'s exact `call_id` and string `output` as
/// an exact-text result without fabricating an error attestation.
#[test]
fn codex_reprojection_maps_custom_tool_call_output_as_exact_text_result() {
    let source_call_id = text("call-custom");
    let output = text("applied");

    assert_codex_payload_projects_one_entry_attesting_no_speaker(
        object_with_members(vec![
            (
                "type",
                ImportedStructuredValue::String(text("custom_tool_call_output")),
            ),
            (
                "call_id",
                ImportedStructuredValue::String(source_call_id.clone()),
            ),
            ("output", ImportedStructuredValue::String(output.clone())),
        ]),
        ImportedTranscriptContent::ToolResult {
            source_call_id: ImportedSourceAttestation::Attested(source_call_id),
            content: ImportedSourceAttestation::Attested(ImportedToolResultValue::Text(output)),
            is_error: ImportedSourceAttestation::NotAttested,
        },
    );
}

/// the Codex reprojection emits one ordered source result block per tool-search element, retaining
/// an object element's exact type attestation and leaving a non-object element unattested.
#[test]
fn codex_reprojection_maps_tool_search_output_as_ordered_blocks() {
    assert_codex_payload_projects_one_entry_attesting_no_speaker(
        object_with_members(vec![
            (
                "type",
                ImportedStructuredValue::String(text("tool_search_output")),
            ),
            (
                "call_id",
                ImportedStructuredValue::String(text("call-search")),
            ),
            (
                "tools",
                ImportedStructuredValue::Array(
                    vec![
                        object(("type", ImportedStructuredValue::String(text("function")))),
                        ImportedStructuredValue::String(text("bare")),
                    ]
                    .into_boxed_slice(),
                ),
            ),
        ]),
        ImportedTranscriptContent::ToolResult {
            source_call_id: ImportedSourceAttestation::Attested(text("call-search")),
            content: ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                vec![
                    ImportedToolResultBlock::SourceResultBlock {
                        source_type: ImportedSourceAttestation::Attested(text("function")),
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
}

/// cloning an unvalidated source value is stack-safe before typed depth rejection.
#[test]
fn unvalidated_structured_clone_is_stack_safe() {
    let value = nested_array(32_768);
    let cloned = value.clone();

    drop(cloned);
}

/// structural equality for unvalidated source values is stack-safe before typed depth rejection.
#[test]
fn unvalidated_structured_equality_is_stack_safe() {
    let value = nested_array(32_768);

    assert_eq!(value, nested_array(32_768));
    assert_ne!(value, nested_array(32_767));
}

/// formatting an unvalidated source value is stack-safe before typed depth rejection.
#[test]
fn unvalidated_structured_debug_is_stack_safe() {
    let value = nested_array(32_768);

    let rendered = format!("{value:?}");
    assert!(rendered.starts_with("Array([Array(["));
    assert!(rendered.ends_with("])])"));
}

/// hashing an unvalidated source value is stack-safe before typed depth rejection.
#[test]
fn unvalidated_structured_hash_is_stack_safe() {
    let value = nested_array(32_768);
    let equal = nested_array(32_768);

    let mut value_hash = DefaultHasher::new();
    value.hash(&mut value_hash);
    let mut equal_hash = DefaultHasher::new();
    equal.hash(&mut equal_hash);

    assert_eq!(value_hash.finish(), equal_hash.finish());
}

fn metadata(role: ImportedSourceAttestation<ImportedSpeaker>) -> ImportedSourceMetadata {
    ImportedSourceMetadata::new(
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        role,
    )
}

struct EntryFixture {
    identity: u128,
    owner: ImportedConversationId,
    position: u64,
    raw_position: u64,
    within_position: u64,
    speaker: ImportedSourceAttestation<ImportedSpeaker>,
    content: ImportedTranscriptContent,
    source: ImportedSourceMetadata,
}

impl EntryFixture {
    fn new(
        identity: u128,
        owner: ImportedConversationId,
        content: ImportedTranscriptContent,
    ) -> Self {
        Self {
            identity,
            owner,
            position: 1,
            raw_position: 1,
            within_position: 1,
            speaker: ImportedSourceAttestation::NotAttested,
            content,
            source: metadata(ImportedSourceAttestation::NotAttested),
        }
    }

    fn position(mut self, position: u64) -> Self {
        self.position = position;
        self
    }

    fn raw_position(mut self, raw_position: u64) -> Self {
        self.raw_position = raw_position;
        self
    }

    fn within_position(mut self, within_position: u64) -> Self {
        self.within_position = within_position;
        self
    }

    fn speaker(mut self, speaker: ImportedSpeaker) -> Self {
        self.speaker = ImportedSourceAttestation::Attested(speaker);
        self.source = metadata(ImportedSourceAttestation::Attested(speaker));
        self
    }

    fn source_speaker(mut self, speaker: ImportedSpeaker) -> Self {
        self.speaker = ImportedSourceAttestation::Attested(speaker);
        self
    }

    fn source(mut self, source: ImportedSourceMetadata) -> Self {
        self.source = source;
        self
    }

    fn build(self) -> ImportedTranscriptEntryInput {
        ImportedTranscriptEntryInput::new(
            entry(self.identity),
            self.owner,
            ImportedTranscriptPosition::try_from_u64(self.position)
                .expect("fixture global position is positive"),
            ImportedRawRecordPosition::try_from_u64(self.raw_position)
                .expect("fixture raw position is positive"),
            ImportedRecordEntryPosition::try_from_u64(self.within_position)
                .expect("fixture within-record position is positive"),
            self.speaker,
            self.content,
            self.source,
        )
    }
}

fn converted() -> ImportedConversation {
    let owner = conversation(1);
    let raw_records = vec![
        ImportedRawSourceRecord::from_converted(
            br#"{"type":"system","content":"before\u0000after"}"#.to_vec(),
            object_with_members(vec![
                (
                    "type",
                    ImportedStructuredValue::String(text("system")),
                ),
                (
                    "content",
                    ImportedStructuredValue::String(text("before\0after")),
                ),
            ]),
        ),
        ImportedRawSourceRecord::from_converted(
            br#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":""},{"type":"tool_use","input":{"n":1}}]}}"#.to_vec(),
            object_with_members(vec![
                (
                    "type",
                    ImportedStructuredValue::String(text("assistant")),
                ),
                (
                    "message",
                    object_with_members(vec![
                        (
                            "role",
                            ImportedStructuredValue::String(text("assistant")),
                        ),
                        (
                            "content",
                            ImportedStructuredValue::Array(
                                vec![
                                    object_with_members(vec![
                                        (
                                            "type",
                                            ImportedStructuredValue::String(text("text")),
                                        ),
                                        (
                                            "text",
                                            ImportedStructuredValue::String(text("")),
                                        ),
                                    ]),
                                    object_with_members(vec![
                                        (
                                            "type",
                                            ImportedStructuredValue::String(text("tool_use")),
                                        ),
                                        (
                                            "input",
                                            object((
                                                "n",
                                                ImportedStructuredValue::Number(
                                                    ImportedJsonNumber::try_new(String::from(
                                                        "1",
                                                    ))
                                                    .expect("fixture number is valid"),
                                                ),
                                            )),
                                        ),
                                    ]),
                                ]
                                .into_boxed_slice(),
                            ),
                        ),
                    ]),
                ),
            ]),
        ),
    ];
    let entries = vec![
        EntryFixture::new(
            2,
            owner,
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(text("system")),
            },
        )
        .build(),
        EntryFixture::new(
            3,
            owner,
            ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text(""))),
        )
        .position(2)
        .raw_position(2)
        .speaker(ImportedSpeaker::Assistant)
        .build(),
        EntryFixture::new(
            4,
            owner,
            ImportedTranscriptContent::ToolCall {
                source_call_id: ImportedSourceAttestation::NotAttested,
                name: ImportedSourceAttestation::NotAttested,
                input: ImportedSourceAttestation::Attested(object((
                    "n",
                    ImportedStructuredValue::Number(
                        ImportedJsonNumber::try_new(String::from("1"))
                            .expect("fixture number is valid"),
                    ),
                ))),
                caller: ImportedSourceAttestation::NotAttested,
            },
        )
        .position(3)
        .raw_position(2)
        .within_position(2)
        .speaker(ImportedSpeaker::Assistant)
        .build(),
    ];
    ImportedConversation::from_converted_records(
        owner,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        raw_records,
        entries,
    )
    .expect("complete converted fixture is valid")
}

/// exact raw records, rich normalized entries, and every imported
/// entry boundary survive one checked immutable aggregate.
#[test]
fn lossless_aggregate_exposes_every_addressable_prefix() {
    let imported = converted();
    assert_eq!(imported.raw_records().len(), 2);
    assert_eq!(
        imported.raw_records()[0].bytes(),
        br#"{"type":"system","content":"before\u0000after"}"#
    );
    assert_eq!(
        imported.raw_records()[0].normalized(),
        &object_with_members(vec![
            ("type", ImportedStructuredValue::String(text("system")),),
            (
                "content",
                ImportedStructuredValue::String(text("before\0after")),
            ),
        ])
    );
    assert_eq!(imported.entries().len(), 3);
    assert_eq!(
        imported.entries()[1].content(),
        &ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text("")))
    );

    let frontiers = imported.frontiers().collect::<Vec<_>>();
    assert_eq!(frontiers.len(), imported.entries().len());
    assert_eq!(
        imported
            .prefix(frontiers[1])
            .expect("aggregate-produced frontier resolves")
            .iter()
            .map(|entry| entry.position().as_u64())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        imported
            .frontier_for_entry(imported.entries()[2].identity())
            .and_then(|frontier| imported.prefix(frontier))
            .map(<[_]>::len),
        Some(3)
    );
}

/// raw bytes and format/order jointly determine stable digests.
#[test]
fn content_hashes_and_source_digest_are_stable_and_ordered() {
    let imported = converted();
    let repeated = converted();
    assert_eq!(imported.source_digest(), repeated.source_digest());
    assert_eq!(
        imported.source_digest().as_bytes(),
        &[
            95, 23, 27, 252, 223, 229, 27, 59, 33, 138, 163, 63, 158, 93, 136, 47, 168, 233, 124,
            3, 8, 217, 172, 182, 134, 109, 156, 227, 239, 156, 211, 83,
        ]
    );
    assert_eq!(
        imported.raw_records()[0].content_hash().as_bytes(),
        &[
            156, 92, 147, 29, 37, 37, 87, 241, 17, 127, 198, 247, 207, 9, 36, 41, 69, 166, 106,
            200, 31, 178, 220, 222, 133, 195, 110, 121, 222, 236, 56, 114,
        ]
    );

    let mut records = imported
        .raw_records()
        .iter()
        .enumerate()
        .map(|(index, record)| {
            ImportedRawSourceRecordReconstitutionInput::new(
                ImportedRawRecordPosition::try_from_u64(
                    u64::try_from(index)
                        .expect("fixture position fits u64")
                        .checked_add(1)
                        .expect("fixture position is positive"),
                )
                .expect("fixture position is positive"),
                record.content_hash(),
                record.conversion_digest(),
                record.bytes().to_vec(),
                record.normalized().clone(),
            )
        })
        .collect::<Vec<_>>();
    records.reverse();
    assert_ne!(
        imported.source_digest(),
        ImportedConversationSourceDigest::derive(imported.format(), &records)
    );
}

#[test]
fn raw_and_conversion_digests_match_the_public_vector() {
    let raw = ImportedRawSourceRecord::from_converted(
        b"{}".to_vec(),
        ImportedStructuredValue::Object(Vec::new().into_boxed_slice()),
    );
    assert_eq!(
        raw.content_hash().as_bytes(),
        &[
            68, 19, 111, 163, 85, 179, 103, 138, 17, 70, 173, 22, 247, 232, 100, 158, 148, 251, 79,
            194, 31, 231, 126, 131, 16, 192, 96, 246, 28, 170, 255, 138,
        ]
    );
    assert_eq!(
        raw.conversion_digest().as_bytes(),
        &[
            61, 6, 248, 52, 193, 194, 253, 219, 191, 69, 71, 22, 218, 48, 154, 243, 147, 209, 85,
            48, 135, 13, 150, 159, 78, 115, 180, 150, 10, 233, 7, 147,
        ]
    );
    let records = vec![ImportedRawSourceRecordReconstitutionInput::new(
        ImportedRawRecordPosition::first(),
        raw.content_hash(),
        raw.conversion_digest(),
        raw.bytes().to_vec(),
        raw.normalized().clone(),
    )];
    assert_eq!(
        ImportedConversationSourceDigest::derive(
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            &records,
        )
        .as_bytes(),
        &[
            184, 54, 163, 251, 0, 70, 92, 44, 126, 192, 28, 242, 196, 178, 201, 136, 69, 203, 201,
            205, 175, 40, 137, 43, 145, 12, 226, 37, 210, 7, 154, 92,
        ]
    );
    assert_eq!(
        ImportedConversationSourceDigest::derive(
            ImportedConversationFormat::ClaudeCodeSessionJsonlV2,
            &records,
        )
        .as_bytes(),
        &[
            17, 122, 201, 89, 149, 113, 247, 255, 40, 57, 6, 154, 229, 37, 34, 54, 215, 158, 161,
            72, 254, 81, 139, 170, 31, 145, 77, 98, 159, 186, 0, 223,
        ]
    );
    assert_eq!(
        ImportedConversationSourceDigest::derive(
            ImportedConversationFormat::CodexRolloutJsonlV1,
            &records,
        )
        .as_bytes(),
        &[
            103, 102, 106, 198, 122, 195, 176, 33, 95, 59, 94, 94, 116, 150, 140, 142, 47, 46, 231,
            87, 71, 24, 244, 119, 145, 115, 105, 108, 236, 246, 36, 223,
        ]
    );
}

#[test]
fn coordinated_normalized_and_entry_corruption_fails_closed() {
    let owner = conversation(1);
    let normalized_message = |value: &str| {
        object_with_members(vec![
            ("type", ImportedStructuredValue::String(text("user"))),
            (
                "message",
                object(("content", ImportedStructuredValue::String(text(value)))),
            ),
        ])
    };
    let converted_raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"user","message":{"content":"original"}}"#.to_vec(),
        normalized_message("original"),
    );
    let raw_records = vec![ImportedRawSourceRecordReconstitutionInput::new(
        ImportedRawRecordPosition::first(),
        converted_raw.content_hash(),
        converted_raw.conversion_digest(),
        converted_raw.bytes().to_vec(),
        normalized_message("changed"),
    )];
    let digest = ImportedConversationSourceDigest::derive(
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        &raw_records,
    );
    let entries = vec![
        EntryFixture::new(
            2,
            owner,
            ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text("changed"))),
        )
        .source_speaker(ImportedSpeaker::User)
        .build(),
    ];
    let error = ImportedConversationReconstitutionInput::new(
        owner,
        owner,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        digest,
        1,
        raw_records,
        1,
        entries,
    )
    .reconstitute()
    .expect_err("coordinated normalized and entry corruption must fail");
    assert_eq!(
        error.failure(),
        ImportedConversationReconstitutionFailure::RawRecordConversionDigestMismatch {
            position: ImportedRawRecordPosition::first(),
        }
    );
}

/// raw-hash corruption fails closed while retaining all
/// typed storage inputs.
#[test]
fn raw_hash_corruption_retains_complete_input() {
    let owner = conversation(1);
    let bytes = br#"{"type":"system"}"#.to_vec();
    let raw_records = vec![ImportedRawSourceRecordReconstitutionInput::new(
        ImportedRawRecordPosition::first(),
        ImportedRawRecordHash::digest(b"different"),
        ImportedRawRecordConversionDigest::from_bytes([0; 32]),
        bytes,
        object(("type", ImportedStructuredValue::String(text("system")))),
    )];
    let digest = ImportedConversationSourceDigest::derive(
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        &raw_records,
    );
    let entries = vec![
        EntryFixture::new(
            2,
            owner,
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(text("system")),
            },
        )
        .build(),
    ];
    let input = ImportedConversationReconstitutionInput::new(
        owner,
        owner,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        digest,
        1,
        raw_records,
        1,
        entries,
    );
    let retained = input.clone();
    let error = input
        .reconstitute()
        .expect_err("stored hash mismatch is corruption");
    assert_eq!(
        error.failure(),
        ImportedConversationReconstitutionFailure::RawRecordHashMismatch {
            position: ImportedRawRecordPosition::first(),
        }
    );
    assert_eq!(error.into_parts().0, retained);
}

#[test]
fn message_content_without_source_speaker_fails_closed() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"user","message":{"content":[]}}"#.to_vec(),
        object(("type", ImportedStructuredValue::String(text("user")))),
    );
    let source = metadata(ImportedSourceAttestation::Attested(ImportedSpeaker::User));
    let wrong_speaker = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::MessageContentAbsent(
            ImportedMessageContentAbsence::EmptyBlockArray,
        ),
    )
    .source(source)
    .build();
    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![wrong_speaker],
        )
        .expect_err("message content requires an attested source speaker")
        .failure(),
        ImportedConversationReconstitutionFailure::MessageSpeakerUnavailable { entry: entry(2) }
    );
}

#[test]
fn reversed_raw_record_mapping_fails_closed() {
    let imported = converted();
    let mut entries = imported
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
        .collect::<Vec<_>>();
    entries[2].raw_record_position = ImportedRawRecordPosition::first();
    let raw_records = imported.raw_records().to_vec();
    assert!(matches!(
        ImportedConversation::from_converted_records(
            conversation(1),
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            raw_records,
            entries,
        )
        .expect_err("entry cannot reverse to an earlier raw record")
        .failure(),
        ImportedConversationReconstitutionFailure::EntryRawRecordPositionMismatch { .. }
    ));
}

#[test]
fn first_entry_cannot_skip_first_raw_record() {
    let owner = conversation(1);
    let raw_records = vec![
        ImportedRawSourceRecord::from_converted(
            br#"{"type":"system"}"#.to_vec(),
            object(("type", ImportedStructuredValue::String(text("system")))),
        ),
        ImportedRawSourceRecord::from_converted(
            br#"{"type":"summary"}"#.to_vec(),
            object(("type", ImportedStructuredValue::String(text("summary")))),
        ),
    ];
    let only_second_record = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::SourceEvent {
            source_type: ImportedSourceAttestation::Attested(text("summary")),
        },
    )
    .raw_position(2)
    .build();

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            raw_records,
            vec![only_second_record],
        )
        .expect_err("the first raw record must produce an entry")
        .failure(),
        ImportedConversationReconstitutionFailure::RawRecordWithoutEntry {
            position: ImportedRawRecordPosition::first(),
        }
    );
}

#[test]
fn source_event_rejects_a_message_record_type() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"user"}"#.to_vec(),
        object(("type", ImportedStructuredValue::String(text("user")))),
    );
    let source_event = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::SourceEvent {
            source_type: ImportedSourceAttestation::Attested(text("user")),
        },
    )
    .build();

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![source_event],
        )
        .expect_err("message discriminators cannot reconstitute as source events")
        .failure(),
        ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch { entry: entry(2) }
    );
}

#[test]
fn message_speaker_must_match_the_raw_record_type() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"user","message":{"role":"assistant"}}"#.to_vec(),
        object(("type", ImportedStructuredValue::String(text("user")))),
    );
    let contradictory_message = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::MessageContentAbsent(
            ImportedMessageContentAbsence::ContentNotAttested,
        ),
    )
    .speaker(ImportedSpeaker::Assistant)
    .build();

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![contradictory_message],
        )
        .expect_err("message speaker must agree with its raw record type")
        .failure(),
        ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch { entry: entry(2) }
    );
}

#[test]
fn empty_raw_source_record_fails_closed() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        Vec::new(),
        object(("type", ImportedStructuredValue::String(text("system")))),
    );
    let source_event = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::SourceEvent {
            source_type: ImportedSourceAttestation::Attested(text("system")),
        },
    )
    .build();

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![source_event],
        )
        .expect_err("a physical JSONL source record cannot be empty")
        .failure(),
        ImportedConversationReconstitutionFailure::EmptyRawRecord {
            position: ImportedRawRecordPosition::first(),
        }
    );
}

#[test]
fn entry_content_must_match_the_complete_normalized_record() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"user","message":{"content":"original"}}"#.to_vec(),
        message_record("user", ImportedStructuredValue::String(text("original"))),
    );
    let changed = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text("changed"))),
    )
    .source_speaker(ImportedSpeaker::User)
    .build();

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![changed],
        )
        .expect_err("stored entry content cannot diverge from its normalized record")
        .failure(),
        ImportedConversationReconstitutionFailure::EntryProjectionMismatch { entry: entry(2) }
    );
}

#[test]
fn entry_metadata_must_match_the_complete_normalized_record() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"system","uuid":"record"}"#.to_vec(),
        object_with_members(vec![
            ("type", ImportedStructuredValue::String(text("system"))),
            ("uuid", ImportedStructuredValue::String(text("record"))),
        ]),
    );
    let missing_metadata = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::SourceEvent {
            source_type: ImportedSourceAttestation::Attested(text("system")),
        },
    )
    .build();

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![missing_metadata],
        )
        .expect_err("stored source metadata cannot diverge from its normalized record")
        .failure(),
        ImportedConversationReconstitutionFailure::EntryProjectionMismatch { entry: entry(2) }
    );
}

#[test]
fn raw_record_entry_count_must_match_its_normalized_projection() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"assistant","message":{"content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}}"#.to_vec(),
        message_record(
            "assistant",
            ImportedStructuredValue::Array(
                vec![
                    object_with_members(vec![
                        ("type", ImportedStructuredValue::String(text("text"))),
                        ("text", ImportedStructuredValue::String(text("one"))),
                    ]),
                    object_with_members(vec![
                        ("type", ImportedStructuredValue::String(text("text"))),
                        ("text", ImportedStructuredValue::String(text("two"))),
                    ]),
                ]
                .into_boxed_slice(),
            ),
        ),
    );
    let incomplete = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text("one"))),
    )
    .source_speaker(ImportedSpeaker::Assistant)
    .build();

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![incomplete],
        )
        .expect_err("every normalized block must have one stored entry")
        .failure(),
        ImportedConversationReconstitutionFailure::RawRecordEntryProjectionMismatch {
            position: ImportedRawRecordPosition::first(),
        }
    );
}

/// converter version 1 retains its original closed block interpretation while version 2 admits
/// source-defined message blocks.
#[test]
fn converter_versions_do_not_reinterpret_source_blocks() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"assistant","message":{"content":[{"type":"future-kind"}]}}"#.to_vec(),
        message_record(
            "assistant",
            ImportedStructuredValue::Array(
                vec![object((
                    "type",
                    ImportedStructuredValue::String(text("future-kind")),
                ))]
                .into_boxed_slice(),
            ),
        ),
    );
    let generic = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::SourceMessageBlock {
            source_type: ImportedSourceAttestation::Attested(text("future-kind")),
        },
    )
    .source_speaker(ImportedSpeaker::Assistant)
    .build();

    let version_two = ImportedConversation::from_converted_records(
        owner,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2,
        vec![raw.clone()],
        vec![generic.clone()],
    )
    .expect("version two admits a source-defined message block");
    assert_eq!(
        version_two.format(),
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2
    );

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![generic],
        )
        .expect_err("version one must retain its original closed interpretation")
        .failure(),
        ImportedConversationReconstitutionFailure::RawRecordProjectionInvalid {
            position: ImportedRawRecordPosition::first(),
        }
    );
}

/// the version boundary also preserves the original closed tool-result block vocabulary.
#[test]
fn converter_versions_do_not_reinterpret_result_blocks() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"future-result"}]}]}}"#.to_vec(),
        message_record(
            "user",
            ImportedStructuredValue::Array(
                vec![object_with_members(vec![
                    (
                        "type",
                        ImportedStructuredValue::String(text("tool_result")),
                    ),
                    (
                        "content",
                        ImportedStructuredValue::Array(
                            vec![object((
                                "type",
                                ImportedStructuredValue::String(text("future-result")),
                            ))]
                            .into_boxed_slice(),
                        ),
                    ),
                ])]
                .into_boxed_slice(),
            ),
        ),
    );
    let generic = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::ToolResult {
            source_call_id: ImportedSourceAttestation::NotAttested,
            content: ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                vec![ImportedToolResultBlock::SourceResultBlock {
                    source_type: ImportedSourceAttestation::Attested(text("future-result")),
                }]
                .into_boxed_slice(),
            )),
            is_error: ImportedSourceAttestation::NotAttested,
        },
    )
    .source_speaker(ImportedSpeaker::User)
    .build();

    ImportedConversation::from_converted_records(
        owner,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2,
        vec![raw.clone()],
        vec![generic.clone()],
    )
    .expect("version two admits a source-defined result block");

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![generic],
        )
        .expect_err("version one must retain its original result-block interpretation")
        .failure(),
        ImportedConversationReconstitutionFailure::RawRecordProjectionInvalid {
            position: ImportedRawRecordPosition::first(),
        }
    );
}

#[test]
fn complete_normalized_record_rejects_129_containers() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"system","nested":[]}"#.to_vec(),
        object_with_members(vec![
            ("type", ImportedStructuredValue::String(text("system"))),
            ("nested", nested_array(128)),
        ]),
    );
    let source_event = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::SourceEvent {
            source_type: ImportedSourceAttestation::Attested(text("system")),
        },
    )
    .build();

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![source_event],
        )
        .expect_err("top-level object plus 128 nested arrays exceeds the bound")
        .failure(),
        ImportedConversationReconstitutionFailure::RawRecordStructuredValueDepthExceeded {
            position: ImportedRawRecordPosition::first(),
        }
    );
}

/// stored structured depth is checked iteratively before any recursive conversion-digest traversal.
#[test]
fn checks_raw_depth_before_recursive_conversion_digest() {
    let owner = conversation(1);
    let bytes = br#"{"type":"system","nested":[]}"#.to_vec();
    let stored_hash = ImportedRawRecordHash::digest(&bytes);
    let raw_records = vec![ImportedRawSourceRecordReconstitutionInput::new(
        ImportedRawRecordPosition::first(),
        stored_hash,
        ImportedRawRecordConversionDigest::from_bytes([0; 32]),
        bytes,
        object_with_members(vec![
            ("type", ImportedStructuredValue::String(text("system"))),
            ("nested", nested_array(32_768)),
        ]),
    )];
    let source_digest = ImportedConversationSourceDigest::derive(
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        &raw_records,
    );
    let error = ImportedConversationReconstitutionInput::new(
        owner,
        owner,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        source_digest,
        1,
        raw_records,
        0,
        Vec::new(),
    )
    .reconstitute()
    .expect_err("excessive stored depth must fail before digest traversal");

    let failure = error.failure();
    drop(error);
    assert_eq!(
        failure,
        ImportedConversationReconstitutionFailure::RawRecordStructuredValueDepthExceeded {
            position: ImportedRawRecordPosition::first(),
        }
    );
}

/// conversion digesting and rejection remain stack-safe for excessive caller-supplied structured
/// depth.
#[test]
fn converted_raw_depth_fails_closed_and_drops_safely() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"system","nested":[]}"#.to_vec(),
        object_with_members(vec![
            ("type", ImportedStructuredValue::String(text("system"))),
            ("nested", nested_array(32_768)),
        ]),
    );
    let source_event = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::SourceEvent {
            source_type: ImportedSourceAttestation::Attested(text("system")),
        },
    )
    .build();

    let error = ImportedConversation::from_converted_records(
        owner,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        vec![raw],
        vec![source_event],
    )
    .expect_err("excessive converted depth must fail without recursive digesting");

    let failure = error.failure();
    drop(error);
    assert_eq!(
        failure,
        ImportedConversationReconstitutionFailure::RawRecordStructuredValueDepthExceeded {
            position: ImportedRawRecordPosition::first(),
        }
    );
}

#[test]
fn entry_carried_structured_value_rejects_129_containers() {
    let owner = conversation(1);
    let raw = ImportedRawSourceRecord::from_converted(
        br#"{"type":"assistant","message":{"content":[{"type":"tool_use","input":null}]}}"#
            .to_vec(),
        message_record(
            "assistant",
            ImportedStructuredValue::Array(
                vec![object_with_members(vec![
                    ("type", ImportedStructuredValue::String(text("tool_use"))),
                    ("input", ImportedStructuredValue::Null),
                ])]
                .into_boxed_slice(),
            ),
        ),
    );
    let excessive = EntryFixture::new(
        2,
        owner,
        ImportedTranscriptContent::ToolCall {
            source_call_id: ImportedSourceAttestation::NotAttested,
            name: ImportedSourceAttestation::NotAttested,
            input: ImportedSourceAttestation::Attested(nested_array(129)),
            caller: ImportedSourceAttestation::NotAttested,
        },
    )
    .source_speaker(ImportedSpeaker::Assistant)
    .build();

    assert_eq!(
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![raw],
            vec![excessive],
        )
        .expect_err("entry-carried structured values obey the same depth bound")
        .failure(),
        ImportedConversationReconstitutionFailure::EntryStructuredValueDepthExceeded {
            entry: entry(2),
        }
    );
}

#[track_caller]
fn assert_valid_json_number(value: &str) {
    assert_eq!(
        ImportedJsonNumber::try_new(String::from(value))
            .expect("fixture is valid")
            .as_str(),
        value
    );
}

#[track_caller]
fn assert_invalid_json_number(value: &str) {
    let error = ImportedJsonNumber::try_new(String::from(value)).expect_err("fixture is invalid");
    assert_eq!(error.value(), value);
}

#[test]
fn imported_json_number_checks_complete_grammar() {
    assert_valid_json_number("0");
    assert_valid_json_number("-0");
    assert_valid_json_number("12");
    assert_valid_json_number("-12.5");
    assert_valid_json_number("1e9");
    assert_valid_json_number("1E-9");

    let empty = ImportedJsonNumber::try_new(String::new()).expect_err("fixture is invalid");
    assert!(empty.value().is_empty());
    assert_invalid_json_number("01");
    assert_invalid_json_number("-");
    assert_invalid_json_number(".1");
    assert_invalid_json_number("1.");
    assert_invalid_json_number("1e");
    assert_invalid_json_number("+1");
    assert_invalid_json_number("NaN");
}

#[test]
fn imported_json_number_debug_redacts_the_source_value() {
    let source_value = "1234567890123456789012345678901234567890e+";
    let error = ImportedJsonNumber::try_new(String::from(source_value))
        .expect_err("fixture has an incomplete exponent");
    assert!(!format!("{error:?}").contains(source_value));
}

/// One Claude Code aggregate whose first record is a `summary` source
/// event and whose second record is one attested user text message.
fn claude_code_summary_fixture(summary: &str, user_text: &str) -> ImportedConversation {
    let owner = conversation(1);
    let raw_records = vec![
        ImportedRawSourceRecord::from_converted(
            format!(r#"{{"type":"summary","summary":"{summary}"}}"#).into_bytes(),
            object_with_members(vec![
                ("type", ImportedStructuredValue::String(text("summary"))),
                ("summary", ImportedStructuredValue::String(text(summary))),
            ]),
        ),
        ImportedRawSourceRecord::from_converted(
            format!(r#"{{"type":"user","message":{{"role":"user","content":"{user_text}"}}}}"#)
                .into_bytes(),
            user_message_record(user_text),
        ),
    ];
    let entries = vec![
        EntryFixture::new(
            2,
            owner,
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(text("summary")),
            },
        )
        .build(),
        EntryFixture::new(
            3,
            owner,
            ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text(user_text))),
        )
        .position(2)
        .raw_position(2)
        .speaker(ImportedSpeaker::User)
        .build(),
    ];
    ImportedConversation::from_converted_records(
        owner,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2,
        raw_records,
        entries,
    )
    .expect("complete summary fixture is valid")
}

/// One complete normalized Claude Code user record whose attested role
/// agrees with its top-level type.
fn user_message_record(user_text: &str) -> ImportedStructuredValue {
    object_with_members(vec![
        ("type", ImportedStructuredValue::String(text("user"))),
        (
            "message",
            object_with_members(vec![
                ("role", ImportedStructuredValue::String(text("user"))),
                ("content", ImportedStructuredValue::String(text(user_text))),
            ]),
        ),
    ])
}

/// One Claude Code aggregate containing exactly one attested user text
/// message and no summary record.
fn claude_code_user_text_fixture(user_text: &str) -> ImportedConversation {
    let owner = conversation(1);
    let raw_records = vec![ImportedRawSourceRecord::from_converted(
        format!(r#"{{"type":"user","message":{{"role":"user","content":{user_text:?}}}}}"#)
            .into_bytes(),
        user_message_record(user_text),
    )];
    let entries = vec![
        EntryFixture::new(
            2,
            owner,
            ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text(user_text))),
        )
        .speaker(ImportedSpeaker::User)
        .build(),
    ];
    ImportedConversation::from_converted_records(
        owner,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV2,
        raw_records,
        entries,
    )
    .expect("complete user-text fixture is valid")
}

/// One Codex aggregate whose first record is a `session_meta` source event
/// carrying the supplied payload members.
fn codex_session_meta_fixture(
    payload: Vec<(&str, ImportedStructuredValue)>,
) -> ImportedConversation {
    let owner = conversation(1);
    let raw_records = vec![ImportedRawSourceRecord::from_converted(
        br#"{"type":"session_meta","payload":{}}"#.to_vec(),
        object_with_members(vec![
            (
                "type",
                ImportedStructuredValue::String(text("session_meta")),
            ),
            ("payload", object_with_members(payload)),
        ]),
    )];
    let entries = vec![
        EntryFixture::new(
            2,
            owner,
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(text("session_meta")),
            },
        )
        .build(),
    ];
    ImportedConversation::from_converted_records(
        owner,
        ImportedConversationFormat::CodexRolloutJsonlV1,
        raw_records,
        entries,
    )
    .expect("complete session-meta fixture is valid")
}

/// The display title prefers the first summary record over user text.
#[test]
fn display_title_derives_from_the_first_claude_code_summary_record() {
    let imported = claude_code_summary_fixture("Fix the flaky import", "unrelated question");

    let title = ImportedConversationDisplayTitle::derive(&imported)
        .expect("summary fixture derives a title");
    assert_eq!(title.as_str(), "Fix the flaky import");
}

/// Without a summary record, the first attested user text supplies the
/// candidate, shaped to its trimmed first line.
#[test]
fn display_title_falls_back_to_shaped_first_attested_user_text() {
    let imported = claude_code_user_text_fixture("  padded question\nsecond line");

    let title = ImportedConversationDisplayTitle::derive(&imported)
        .expect("user-text fixture derives a title");
    assert_eq!(title.as_str(), "padded question");
}

/// A whitespace-only summary is exhausted and the user text is tried next.
#[test]
fn display_title_exhausts_a_blank_summary_candidate() {
    let imported = claude_code_summary_fixture("  ", "fallback question");

    let title = ImportedConversationDisplayTitle::derive(&imported)
        .expect("fallback candidate derives a title");
    assert_eq!(title.as_str(), "fallback question");
}

/// A candidate longer than the bound truncates to the first 256 scalars.
#[test]
fn display_title_truncates_to_the_scalar_bound() {
    let imported = claude_code_user_text_fixture(&"x".repeat(300));

    let title = ImportedConversationDisplayTitle::derive(&imported)
        .expect("oversized candidate still derives a title");
    assert_eq!(title.as_str(), "x".repeat(256));
}

/// A conversation with no summary and no attested user text derives
/// nothing rather than fabricating a title.
#[test]
fn display_title_is_underivable_without_any_candidate() {
    let imported = codex_session_meta_fixture(vec![(
        "cwd",
        ImportedStructuredValue::String(text("/workspace/rollout")),
    )]);

    assert_eq!(ImportedConversationDisplayTitle::derive(&imported), None);
}

/// A Codex `session_meta` payload title outranks its instructions.
#[test]
fn display_title_prefers_codex_session_meta_title_over_instructions() {
    let imported = codex_session_meta_fixture(vec![
        (
            "instructions",
            ImportedStructuredValue::String(text("long standing instructions")),
        ),
        (
            "title",
            ImportedStructuredValue::String(text("Rollout title")),
        ),
    ]);

    let title = ImportedConversationDisplayTitle::derive(&imported)
        .expect("titled session-meta fixture derives a title");
    assert_eq!(title.as_str(), "Rollout title");
}

/// A Codex `session_meta` without a title falls back to instructions.
#[test]
fn display_title_derives_from_codex_session_meta_instructions() {
    let imported = codex_session_meta_fixture(vec![(
        "instructions",
        ImportedStructuredValue::String(text("Review the queue daily")),
    )]);

    let title = ImportedConversationDisplayTitle::derive(&imported)
        .expect("instruction session-meta fixture derives a title");
    assert_eq!(title.as_str(), "Review the queue daily");
}

#[test]
fn display_title_construction_rejects_empty_text() {
    assert_eq!(
        ImportedConversationDisplayTitle::try_new(String::new()),
        Err(ImportedConversationDisplayTitleError::Empty)
    );
}

#[test]
fn display_title_construction_rejects_nul() {
    assert_eq!(
        ImportedConversationDisplayTitle::try_new(String::from("a\0b")),
        Err(ImportedConversationDisplayTitleError::ContainsNul)
    );
}

#[test]
fn display_title_construction_rejects_line_breaks() {
    assert_eq!(
        ImportedConversationDisplayTitle::try_new(String::from("a\nb")),
        Err(ImportedConversationDisplayTitleError::ContainsLineBreak)
    );
    assert_eq!(
        ImportedConversationDisplayTitle::try_new(String::from("a\rb")),
        Err(ImportedConversationDisplayTitleError::ContainsLineBreak)
    );
}

#[test]
fn display_title_construction_rejects_excess_scalars() {
    assert_eq!(
        ImportedConversationDisplayTitle::try_new("x".repeat(257)),
        Err(ImportedConversationDisplayTitleError::ExceedsMaxScalars { scalars: 257 })
    );
}

#[test]
fn display_title_construction_rejects_edge_whitespace() {
    assert_eq!(
        ImportedConversationDisplayTitle::try_new(String::from(" title")),
        Err(ImportedConversationDisplayTitleError::UntrimmedEdgeWhitespace)
    );
    assert_eq!(
        ImportedConversationDisplayTitle::try_new(String::from("title\t")),
        Err(ImportedConversationDisplayTitleError::UntrimmedEdgeWhitespace)
    );
}

/// Stored derived shapes reconstruct exactly through checked construction.
#[test]
fn display_title_construction_accepts_a_derived_shape() {
    let title = ImportedConversationDisplayTitle::try_new(String::from("Fix the flaky import"))
        .expect("derived shape is valid");
    assert_eq!(title.as_str(), "Fix the flaky import");
    assert_eq!(title.clone().into_string(), "Fix the flaky import");
}
