use std::{
    io::{self, Write},
    path::Path,
    str::FromStr,
};

use expect_test::expect;
use rust_decimal::Decimal;
use signalbox_process_protocol::{
    BillingRateVersion, BoundChildAction, CanonicalDollarAmount, CanonicalU64, CanonicalUuid,
    ContentFragment, CurrentModelCall, CurrentModelCallState, DelegationOutcome, DelegationPolicy,
    DelegationProvenance, DelegationReason, DelegationWaitMode, DescendantTerminationScope,
    ErrorCode, ErrorDetail, FailedModelCallDisposition, FailedTerminalModelCall,
    ImportedContentKind, ImportedSourceSpeaker, ImportedSpeaker, ImportedTextPreview, InputContent,
    MetadataActor, MetadataLastWriter, ModelCallCostLabel, ModelCallDollarCost, ModelCallState,
    ModelCallTokenUsage, OperatorStatusLifecycleDeadlineViolationMessage,
    OperatorStatusLifecycleState, OperatorStatusLifecycleWeekMessage, OperatorStatusMessage,
    OperatorStatusUnavailableComponentMessage, ReviewDiffSide, ReviewFindingInput,
    ReviewFindingSnapshot, ReviewFindingStatus, ReviewSeverity, ReviewTargetSnapshot,
    ReviewTargetSubject, RunnerCapabilityClass, RunnerConnectionHealth,
    RunnerCredentialProfileName, RunnerPlacementRevision, RunnerProjection,
    RunnerProjectionSelector, RunnerProjectionState, RunnerRepositoryKey, RunnerSandboxProfile,
    RunnerStateTransitionState, RunnerWorkingDirectory, ServerMessage, SessionEvent,
    ToolApprovalEventDecider, ToolApprovalEventDecision, TranscriptEntry, TranscriptTextEntry,
    TurnState, UsageProvenance, UserInputContent,
};
use uuid::Uuid;

use super::{
    ConversationRow, CostAggregateKey, DiskCostTotals, ImportedEntryRow, Output,
    SessionMetadataRow, SnapshotSelection, TextField, control_safe, last_writer_actor_label,
};
use crate::{
    error::ClientError,
    transcript::{SnapshotEntry, SnapshotEntryKind, SnapshotIdentitySet, TranscriptSnapshot},
};

#[test]
fn review_target_names_an_absent_base_revision() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .review_target(&review_target_snapshot(None))
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        target=00000000-0000-0000-0000-000000000001 subject=commit parent=-
        provider=example-host
        repository=example/repository
        head_revision=head
        base_revision_present=false
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn review_target_preserves_a_literal_dash_base_revision() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .review_target(&review_target_snapshot(Some(String::from("-"))))
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        target=00000000-0000-0000-0000-000000000001 subject=commit parent=-
        provider=example-host
        repository=example/repository
        head_revision=head
        base_revision_present=true
        base_revision=-
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn review_finding_renders_its_complete_snapshot() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .review_finding(&review_finding_snapshot())
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        finding=00000000-0000-0000-0000-000000000004 target=00000000-0000-0000-0000-000000000001 run=00000000-0000-0000-0000-000000000002 pass=00000000-0000-0000-0000-000000000003 status=open events=2 line_start=7 line_end=9 diff_side=right severity=high is_real_confidence=9000 severity_label_confidence=8500
        file_path=src/lib.rs
        title=Retain evidence
        body=First line\u{a}Second line
        category=correctness
        recommended_fix_present=true
        recommended_fix=Bind the exact\u{a}pass.
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn terminal_safe_text_preserves_line_feed_and_escapes_c0_del_and_c1() {
    assert_eq!(
        control_safe("a\n\t\u{1b}\u{7f}\u{85}z", TextField::Flowing),
        "a\n\\u{9}\\u{1b}\\u{7f}\\u{85}z"
    );
    assert_eq!(
        control_safe("café\u{1f980}", TextField::Flowing),
        "café\u{1f980}"
    );
}

#[test]
fn terminal_safe_trailing_field_escapes_line_feed_and_keeps_its_spaces() {
    assert_eq!(
        control_safe("a\n\t\u{1b}\u{7f}\u{85}z", TextField::TrailingOnLine),
        "a\\u{a}\\u{9}\\u{1b}\\u{7f}\\u{85}z"
    );
    assert_eq!(
        control_safe("café, and a space\u{1f980}", TextField::TrailingOnLine),
        "café, and a space\u{1f980}"
    );
}

#[test]
fn provider_text_delta_is_terminal_safe_and_flushed_immediately() {
    let session_id = wire_uuid(1);
    let turn_id = wire_uuid(2);
    let model_call_id = wire_uuid(3);
    let part_index = 4;
    let mut stdout = FlushWriter::default();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .provider_text_delta(
            session_id,
            turn_id,
            model_call_id,
            part_index,
            "first\nforged event\u{1b}",
        )
        .expect("in-memory output cannot fail");

    assert_eq!(
        String::from_utf8(stdout.bytes).expect("rendered output is UTF-8"),
        format!(
            "provider_text_delta session={session_id} turn={turn_id} \
             call={model_call_id} part={part_index} \
             content=first\\u{{a}}forged event\\u{{1b}}\n"
        )
    );
    assert_eq!(stdout.flushes, 1);
    assert!(stderr.is_empty());
}

#[test]
fn operator_status_renders_all_sections_and_explains_omitted_usage() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    {
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        output
            .operator_status_counts(super::OperatorStatusPresentationCounts {
                lifecycle_weeks: 1,
                lifecycle_deadline_violations: 1,
            })
            .expect("in-memory output cannot fail");
        output
            .operator_status_item(&ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::UnavailableComponent(Box::new(
                    OperatorStatusUnavailableComponentMessage {
                        component: "adapter:codex_cli".to_owned(),
                        cause: "codex_cli_pin_mismatch".to_owned(),
                    },
                )),
            )))
            .expect("in-memory output cannot fail");
        output
            .operator_status_item(&ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::LifecycleWeek(Box::new(
                    OperatorStatusLifecycleWeekMessage {
                        week_start_date: String::from("2026-08-31"),
                        completion_failure_numerator: CanonicalU64::new(3),
                        completion_failure_denominator: CanonicalU64::new(40),
                        failed_unknown_count: CanonicalU64::new(1),
                        overflow_numerator: CanonicalU64::new(5),
                        overflow_denominator: CanonicalU64::new(44),
                        finish_given_overflow_numerator: CanonicalU64::new(4),
                        wall_numerator: CanonicalU64::new(0),
                        wall_denominator: CanonicalU64::new(38),
                        wall_occurrence_count: CanonicalU64::new(0),
                        classified_terminal_turn_count: CanonicalU64::new(980),
                        terminal_turn_count: CanonicalU64::new(985),
                        classified_known_failed_call_count: CanonicalU64::new(91),
                        known_failed_call_count: CanonicalU64::new(95),
                    },
                )),
            )))
            .expect("in-memory output cannot fail");
        output
            .operator_status_item(&ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::LifecycleDeadlineViolation(Box::new(
                    OperatorStatusLifecycleDeadlineViolationMessage {
                        session_id: wire_uuid(6),
                        state: OperatorStatusLifecycleState::Parked,
                        deadline_missing: false,
                        expired_for_seconds: Some(CanonicalU64::new(90)),
                    },
                )),
            )))
            .expect("in-memory output cannot fail");
        output
            .operator_status_model_usage_omitted()
            .expect("in-memory output cannot fail");
    }

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        status lifecycle_weeks=1 nonterminal_past_deadline=1
        unavailable_component component=adapter:codex_cli cause=codex_cli_pin_mismatch
        lifecycle_week week=2026-08-31 completion_failure=3/40@75000ppm failed_unknown=1/40@25000ppm overflow=5/44@113636ppm finish_given_overflow=4/5@800000ppm wall=0/38@0ppm wall_occurrences=0 turn_cause_completeness=980/985@994923ppm model_call_cause_completeness=91/95@957894ppm
        nonterminal_past_deadline session=00000000-0000-0000-0000-000000000006 state=parked deadline=armed expired=1m30s
        model_usage=omitted reason=no_cheap_status_aggregate
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn terminal_safe_delimited_field_escapes_the_space_and_comma_that_delimit_it() {
    assert_eq!(
        control_safe("a\n\t\u{1b}\u{7f}\u{85}z", TextField::DelimitedOnLine),
        "a\\u{a}\\u{9}\\u{1b}\\u{7f}\\u{85}z"
    );
    assert_eq!(
        control_safe("café, and a space\u{1f980}", TextField::DelimitedOnLine),
        "café\\u{2c}\\u{20}and\\u{20}a\\u{20}space\u{1f980}"
    );
}

#[test]
fn terminal_safe_delimited_field_distinguishes_written_escape_text_from_an_escape() {
    let comma = control_safe(",", TextField::DelimitedOnLine);
    let escape_text_for_a_comma = control_safe("\\u{2c}", TextField::DelimitedOnLine);

    assert_eq!(comma, "\\u{2c}");
    assert_eq!(escape_text_for_a_comma, "\\u{5c}u{2c}");
    assert_ne!(comma, escape_text_for_a_comma);
}

#[test]
fn scan_failure_reason_cannot_forge_an_outcome_line() {
    let error = ClientError::remote(
        ErrorCode::Unavailable,
        String::from("first line\nscan_summary imported=99 already_imported=99 skipped=0"),
        ErrorDetail::none(),
    );
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .conversation_import_scan_skipped(Path::new("conversation.jsonl"), &error)
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        skipped path="conversation.jsonl" reason=unavailable: first line\u{a}scan_summary imported=99 already_imported=99 skipped=0
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn imported_renders_a_previewed_attested_text_entry() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .imported_conversation_entry(&ImportedEntryRow {
            position: 2,
            imported_entry_id: wire_uuid(7),
            source_speaker: ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::Assistant,
            },
            content_kind: ImportedContentKind::Text,
            text_preview: Some(&ImportedTextPreview::of_exact_text("synthetic answer")),
        })
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        position=2 imported_entry=00000000-0000-0000-0000-000000000007 speaker=assistant kind=text truncated=false text=synthetic answer
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn imported_renders_a_nontext_entry_without_preview_fields() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .imported_conversation_entry(&ImportedEntryRow {
            position: 1,
            imported_entry_id: wire_uuid(7),
            source_speaker: ImportedSourceSpeaker::NotAttested {},
            content_kind: ImportedContentKind::SourceEvent,
            text_preview: None,
        })
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        position=1 imported_entry=00000000-0000-0000-0000-000000000007 speaker=unattested kind=source_event
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn imported_preview_text_cannot_forge_another_entry_row() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .imported_conversation_entry(&ImportedEntryRow {
            position: 3,
            imported_entry_id: wire_uuid(7),
            source_speaker: ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::User,
            },
            content_kind: ImportedContentKind::Text,
            text_preview: Some(&ImportedTextPreview::of_exact_text(
                "forged\nposition=9 imported_entry=00000000-0000-0000-0000-000000000008",
            )),
        })
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        position=3 imported_entry=00000000-0000-0000-0000-000000000007 speaker=user kind=text truncated=false text=forged\u{a}position=9 imported_entry=00000000-0000-0000-0000-000000000008
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn imported_names_its_entry_count_as_the_greatest_selectable_position() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .imported_conversation_entry_count(2)
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        entry_count=2
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn continue_prints_the_resolved_latest_position_before_its_command() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .resolved_through_position(2)
        .expect("in-memory output cannot fail");

    assert!(stdout.is_empty());
    expect![[r#"
        through_position=2
    "#]]
    .assert_eq(&String::from_utf8(stderr).expect("rendered output is UTF-8"));
}

#[test]
fn search_renders_one_complete_written_metadata_row() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .session_metadata_summary(&SessionMetadataRow {
            session_id: wire_uuid(1),
            defaults_version: 2,
            selection: "model=00000000-0000-0000-0000-000000000003",
            dangerous_tool_auto_approval: true,
            archived: true,
            last_writer: Some(MetadataLastWriter::new(
                CanonicalU64::new(1_753_484_400_000_000),
                MetadataActor::User {},
            )),
            tags: &[String::from("daily"), String::from("plan")],
            title: Some("Active plan"),
        })
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        00000000-0000-0000-0000-000000000001 archived=true defaults_version=2 model=00000000-0000-0000-0000-000000000003 dangerous_tool_auto_approval=approve-all last_writer=user updated_at_unix_micros=1753484400000000 tags=daily,plan title=Active plan
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

/// One written last-writer stamp carrying the actor under test; the
/// timestamp is fixture plumbing the label never reads.
fn written_by(actor: MetadataActor) -> Option<MetadataLastWriter> {
    Some(MetadataLastWriter::new(CanonicalU64::new(1), actor))
}

#[test]
fn search_names_every_last_writer_actor_the_wire_can_carry() {
    assert_eq!(
        last_writer_actor_label(written_by(MetadataActor::User {})),
        "user"
    );
    assert_eq!(
        last_writer_actor_label(written_by(MetadataActor::Model {
            turn_id: wire_uuid(2)
        })),
        "model"
    );
    assert_eq!(
        last_writer_actor_label(written_by(MetadataActor::Recovery {})),
        "recovery"
    );
    assert_eq!(
        last_writer_actor_label(written_by(MetadataActor::Tool {
            tool_request_id: wire_uuid(3)
        })),
        "tool"
    );
    assert_eq!(last_writer_actor_label(None), "none");
}

#[test]
fn search_renders_the_unwritten_metadata_row_with_named_absences() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .session_metadata_summary(&SessionMetadataRow {
            session_id: wire_uuid(1),
            defaults_version: 1,
            selection: "alias=00000000-0000-0000-0000-000000000002",
            dangerous_tool_auto_approval: false,
            archived: false,
            last_writer: None,
            tags: &[],
            title: None,
        })
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        00000000-0000-0000-0000-000000000001 archived=false defaults_version=1 alias=00000000-0000-0000-0000-000000000002 dangerous_tool_auto_approval=disabled last_writer=none updated_at_unix_micros=none tags= title=
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn search_title_and_tags_cannot_forge_another_row() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .session_metadata_summary(&SessionMetadataRow {
            session_id: wire_uuid(1),
            defaults_version: 1,
            selection: "model=00000000-0000-0000-0000-000000000003",
            dangerous_tool_auto_approval: false,
            archived: false,
            last_writer: None,
            tags: &[String::from("first\nsecond")],
            title: Some("forged\n00000000-0000-0000-0000-000000000002 archived=false"),
        })
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        00000000-0000-0000-0000-000000000001 archived=false defaults_version=1 model=00000000-0000-0000-0000-000000000003 dangerous_tool_auto_approval=disabled last_writer=none updated_at_unix_micros=none tags=first\u{a}second title=forged\u{a}00000000-0000-0000-0000-000000000002 archived=false
    "#]]
    .assert_eq(&rendered);
    assert_eq!(rendered.lines().count(), 1);
    assert!(stderr.is_empty());
}

#[test]
fn conversations_render_origin_tagged_native_and_imported_rows() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    output
        .conversation_summary(&ConversationRow::Native {
            session_id: wire_uuid(1),
            archived: true,
            defaults_version: 2,
            title: Some("Active plan"),
        })
        .expect("in-memory output cannot fail");
    output
        .conversation_summary(&ConversationRow::Imported {
            imported_conversation_id: wire_uuid(2),
            format: "codex-rollout-jsonl-v1",
            entry_count: 7,
            title: None,
        })
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        origin=native session_id=00000000-0000-0000-0000-000000000001 archived=true defaults_version=2 title=Active plan
        origin=imported imported_conversation_id=00000000-0000-0000-0000-000000000002 format=codex-rollout-jsonl-v1 entry_count=7 title=
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn conversation_title_cannot_forge_another_row() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .conversation_summary(&ConversationRow::Imported {
            imported_conversation_id: wire_uuid(1),
            format: "claude-code-session-jsonl-v2",
            entry_count: 1,
            title: Some("forged\norigin=native session_id=00000000-0000-0000-0000-000000000002"),
        })
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        origin=imported imported_conversation_id=00000000-0000-0000-0000-000000000001 format=claude-code-session-jsonl-v2 entry_count=1 title=forged\u{a}origin=native session_id=00000000-0000-0000-0000-000000000002
    "#]]
    .assert_eq(&rendered);
    assert_eq!(rendered.lines().count(), 1);
    assert!(stderr.is_empty());
}

#[test]
fn conversation_cursor_is_printed_to_standard_error() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .next_conversation_cursor("imported", wire_uuid(3))
        .expect("in-memory output cannot fail");

    assert!(stdout.is_empty());
    assert_eq!(
        String::from_utf8(stderr).expect("rendered output is UTF-8"),
        "next_after=imported:00000000-0000-0000-0000-000000000003\n"
    );
}

#[test]
fn search_tags_state_their_exact_boundaries() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .session_metadata_summary(&SessionMetadataRow {
            session_id: wire_uuid(1),
            defaults_version: 1,
            selection: "model=00000000-0000-0000-0000-000000000003",
            dangerous_tool_auto_approval: false,
            archived: false,
            last_writer: None,
            tags: &[String::from("one,tag title=forged"), String::from("second")],
            title: Some("Active plan"),
        })
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        00000000-0000-0000-0000-000000000001 archived=false defaults_version=1 model=00000000-0000-0000-0000-000000000003 dangerous_tool_auto_approval=disabled last_writer=none updated_at_unix_micros=none tags=one\u{2c}tag\u{20}title=forged,second title=Active plan
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn search_prints_its_continuation_cursor_to_standard_error() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .next_page_cursor(wire_uuid(1))
        .expect("in-memory output cannot fail");

    assert!(stdout.is_empty());
    assert_eq!(
        String::from_utf8(stderr).expect("rendered output is UTF-8"),
        "next_after_session_id=00000000-0000-0000-0000-000000000001\n"
    );
}

#[test]
fn raw_assistant_text_flushes_without_adding_a_delimiter() {
    let mut stdout = FlushWriter::default();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, true);
    output
        .assistant_text_fragment("ok", true, false)
        .expect("in-memory output cannot fail");
    assert_eq!(stdout.bytes, b"ok");
    assert_eq!(stdout.flushes, 1);
    assert!(stderr.is_empty());
}

#[test]
fn followed_snapshot_renders_queued_content_before_adopting_its_cursor() {
    let turn_id = wire_uuid(1);
    let accepted_input_id = wire_uuid(2);
    let mut snapshot = TranscriptSnapshot::from_messages(
        9,
        [ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Queued {
                accepted_input_id,
                content: UserInputContent::text("queued user text".to_owned()),
            },
        }],
    )
    .expect("test snapshot must spool");
    let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .followed_snapshot(&mut snapshot, &mut displayed)
        .expect("queued snapshot must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    assert!(rendered.contains("state=queued"));
    assert!(rendered.contains("queued user text"));
    assert!(stderr.is_empty());
}

#[test]
fn session_summary_renders_its_complete_runner_projection() {
    let projection = RunnerProjection::try_new(
        RunnerProjectionSelector::CapabilityClass {
            name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))
                .expect("the fixture capability class is valid"),
        },
        Some(wire_uuid(2)),
        RunnerPlacementRevision::try_new(3).expect("the fixture placement revision is positive"),
        RunnerSandboxProfile::WorkspaceRestricted,
        Some(
            RunnerCredentialProfileName::try_new(String::from("readonly"))
                .expect("the fixture credential profile is valid"),
        ),
        Some(
            RunnerRepositoryKey::try_new(String::from("signalbox"))
                .expect("the fixture repository key is valid"),
        ),
        Some(
            RunnerWorkingDirectory::try_new(String::from("workspace root\nproject"))
                .expect("the fixture working directory is valid"),
        ),
        Some(RunnerConnectionHealth::Suspect),
        RunnerProjectionState::Pinned,
    )
    .expect("the fixture projection is coherent");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    Output::new(&mut stdout, &mut stderr, false)
        .session_summary(
            wire_uuid(1),
            4,
            "model=alias alias=fast",
            2,
            "placement=pathless",
            Some(&projection),
        )
        .expect("in-memory output cannot fail");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        00000000-0000-0000-0000-000000000001 defaults_version=4 model=alias alias=fast placement_version=2 placement=pathless runner_selector=capability_class runner_selector_capability=linux.workspace runner=00000000-0000-0000-0000-000000000002 runner_placement_revision=3 runner_sandbox=workspace_restricted runner_credential_profile=readonly runner_repository=signalbox runner_working_directory=workspace\u{20}root\u{a}project runner_connection_health=suspect runner_state=pinned
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn followed_snapshot_renders_its_complete_authoritative_runner_projection() {
    let projection = RunnerProjection::try_new(
        RunnerProjectionSelector::CapabilityClass {
            name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))
                .expect("the fixture capability class is valid"),
        },
        Some(wire_uuid(2)),
        RunnerPlacementRevision::try_new(3).expect("the fixture placement revision is positive"),
        RunnerSandboxProfile::WorkspaceRestricted,
        Some(
            RunnerCredentialProfileName::try_new(String::from("readonly"))
                .expect("the fixture credential profile is valid"),
        ),
        Some(
            RunnerRepositoryKey::try_new(String::from("signalbox"))
                .expect("the fixture repository key is valid"),
        ),
        Some(
            RunnerWorkingDirectory::try_new(String::from("workspace root\nproject"))
                .expect("the fixture working directory is valid"),
        ),
        Some(RunnerConnectionHealth::Suspect),
        RunnerProjectionState::Pinned,
    )
    .expect("the fixture projection is coherent");
    let mut snapshot = TranscriptSnapshot::from_messages_with_runner(
        9,
        Some(projection),
        std::iter::empty::<ServerMessage>(),
    )
    .expect("test snapshot must spool");
    let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .followed_snapshot(&mut snapshot, &mut displayed)
        .expect("runner snapshot must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        runner_snapshot selector=capability_class selector_capability=linux.workspace runner=00000000-0000-0000-0000-000000000002 placement_revision=3 sandbox=workspace_restricted credential_profile=readonly repository=signalbox working_directory=workspace\u{20}root\u{a}project connection_health=suspect state=pinned
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn imported_snapshot_renders_attested_text() {
    let mut snapshot = TranscriptSnapshot::from_messages(
        9,
        [
            ServerMessage::TranscriptTextEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(1),
                entry_id: wire_uuid(2),
                entry: TranscriptTextEntry::Imported {
                    imported_conversation_id: wire_uuid(3),
                    imported_entry_id: wire_uuid(4),
                    source_speaker: ImportedSourceSpeaker::Attested {
                        speaker: ImportedSpeaker::User,
                    },
                },
            },
            ServerMessage::TranscriptContent {
                entry_index: CanonicalU64::new(0),
                fragment_index: CanonicalU64::new(0),
                final_fragment: true,
                content_fragment: ContentFragment::try_new("exact imported text".to_owned())
                    .expect("short content is valid"),
            },
        ],
    )
    .expect("test snapshot must spool");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .snapshot(&mut snapshot)
        .expect("imported snapshot must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        imported_user imported_conversation=00000000-0000-0000-0000-000000000003 imported_entry=00000000-0000-0000-0000-000000000004 source=00000000-0000-0000-0000-000000000001 entry=00000000-0000-0000-0000-000000000002
        exact imported text
        usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn snapshot_user_entry_renders_canonical_parts_on_one_line() {
    let mut snapshot = TranscriptSnapshot::from_messages(
        9,
        [ServerMessage::TranscriptUserEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: wire_uuid(1),
            entry_id: wire_uuid(2),
            accepted_input_id: wire_uuid(3),
            turn_id: wire_uuid(4),
            content: UserInputContent::text("first\nsecond".to_owned()),
        }],
    )
    .expect("test snapshot must spool");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .snapshot(&mut snapshot)
        .expect("user snapshot must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    assert!(rendered.starts_with(
        "user_content source_session=00000000-0000-0000-0000-000000000001 entry=00000000-0000-0000-0000-000000000002 accepted_input=00000000-0000-0000-0000-000000000003 turn=00000000-0000-0000-0000-000000000004 parts=[{\"type\":\"text\",\"text\":\"first\\nsecond\"}]\n"
    ));
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.starts_with("user_content "))
            .count(),
        1
    );
    assert!(stderr.is_empty());
}

#[test]
fn imported_snapshot_renders_conservative_nontext() {
    let mut snapshot = TranscriptSnapshot::from_messages(
        9,
        [ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: wire_uuid(1),
            entry_id: wire_uuid(5),
            entry: TranscriptEntry::Imported {
                imported_conversation_id: wire_uuid(3),
                imported_entry_id: wire_uuid(6),
                source_speaker: ImportedSourceSpeaker::NotAttested {},
                content_kind: ImportedContentKind::ToolCall,
            },
        }],
    )
    .expect("test snapshot must spool");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .snapshot(&mut snapshot)
        .expect("imported snapshot must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        imported_speaker_unattested kind=tool_call imported_conversation=00000000-0000-0000-0000-000000000003 imported_entry=00000000-0000-0000-0000-000000000006 source=00000000-0000-0000-0000-000000000001 entry=00000000-0000-0000-0000-000000000005
        usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn delegation_snapshot_renders_task_message_and_background_result() {
    let mut snapshot = TranscriptSnapshot::from_messages(
        9,
        [
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(2),
                entry_id: wire_uuid(3),
                entry: TranscriptEntry::DelegatedTask {
                    spawning_request_id: wire_uuid(4),
                    parent_session_id: wire_uuid(1),
                    parent_turn_id: wire_uuid(5),
                    content: String::from("inspect the durable result"),
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(1),
                source_session_id: wire_uuid(2),
                entry_id: wire_uuid(6),
                entry: TranscriptEntry::DelegationMessage {
                    spawning_request_id: wire_uuid(4),
                    message_id: wire_uuid(7),
                    sender_session_id: wire_uuid(1),
                    recipient_session_id: wire_uuid(2),
                    ordinal: CanonicalU64::new(2),
                    delivery_sequence: CanonicalU64::new(1),
                    content: String::from("continue with the checked input"),
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(2),
                source_session_id: wire_uuid(1),
                entry_id: wire_uuid(8),
                entry: TranscriptEntry::DelegationResult {
                    await_request_id: wire_uuid(9),
                    spawning_request_id: wire_uuid(4),
                    child_session_id: wire_uuid(2),
                    mode: DelegationWaitMode::Background,
                    delivery_sequence: Some(CanonicalU64::new(2)),
                    outcome: DelegationOutcome::Returned,
                    content: Some(String::from("checked result")),
                    reason: DelegationReason::ChildCompleted,
                    provenance: DelegationProvenance::ChildTurn {
                        child_session_id: wire_uuid(2),
                        child_turn_id: wire_uuid(10),
                    },
                },
            },
        ],
    )
    .expect("test snapshot must spool");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .snapshot(&mut snapshot)
        .expect("delegation snapshot must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        delegated_task spawning_request=00000000-0000-0000-0000-000000000004 parent_session=00000000-0000-0000-0000-000000000001 parent_turn=00000000-0000-0000-0000-000000000005 content=inspect the durable result source=00000000-0000-0000-0000-000000000002 entry=00000000-0000-0000-0000-000000000003
        delegation_message spawning_request=00000000-0000-0000-0000-000000000004 message=00000000-0000-0000-0000-000000000007 sender=00000000-0000-0000-0000-000000000001 recipient=00000000-0000-0000-0000-000000000002 ordinal=2 delivery_sequence=1 content=continue with the checked input source=00000000-0000-0000-0000-000000000002 entry=00000000-0000-0000-0000-000000000006
        delegation_result await_request=00000000-0000-0000-0000-000000000009 spawning_request=00000000-0000-0000-0000-000000000004 child=00000000-0000-0000-0000-000000000002 mode=background delivery_sequence=2 outcome=returned content=checked result reason=child_completed provenance=child_turn:00000000-0000-0000-0000-000000000002:00000000-0000-0000-0000-00000000000a source=00000000-0000-0000-0000-000000000001 entry=00000000-0000-0000-0000-000000000008
        usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn terminal_reread_excludes_material_from_later_buffered_events() {
    let selected_turn = wire_uuid(1);
    let selected_call = wire_uuid(2);
    let later_turn = wire_uuid(3);
    let later_call = wire_uuid(4);
    let mut snapshot = TranscriptSnapshot::from_messages(
        12,
        [
            ServerMessage::TranscriptTextEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(11),
                entry: TranscriptTextEntry::Assistant {
                    turn_id: selected_turn,
                    model_call_id: selected_call,
                },
            },
            ServerMessage::TranscriptContent {
                entry_index: CanonicalU64::new(0),
                fragment_index: CanonicalU64::new(0),
                final_fragment: true,
                content_fragment: ContentFragment::try_new("selected reply".to_owned())
                    .expect("short content is valid"),
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(1),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(12),
                entry: TranscriptEntry::TurnCompleted {
                    turn_id: selected_turn,
                },
            },
            ServerMessage::TranscriptTextEntry {
                entry_index: CanonicalU64::new(2),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(13),
                entry: TranscriptTextEntry::Assistant {
                    turn_id: later_turn,
                    model_call_id: later_call,
                },
            },
            ServerMessage::TranscriptContent {
                entry_index: CanonicalU64::new(2),
                fragment_index: CanonicalU64::new(0),
                final_fragment: true,
                content_fragment: ContentFragment::try_new("later reply".to_owned())
                    .expect("short content is valid"),
            },
        ],
    )
    .expect("test snapshot must spool");
    let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .terminal_material(
            &mut snapshot,
            &mut displayed,
            SnapshotSelection::Completed {
                turn_id: selected_turn,
                model_call_id: selected_call,
                terminal_entry_id: wire_uuid(12),
            },
        )
        .expect("selected terminal material must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    assert!(rendered.contains("selected reply"));
    assert!(rendered.contains("turn_completed"));
    assert!(!rendered.contains("later reply"));
    assert!(!rendered.contains(&later_turn.to_string()));
    assert!(stderr.is_empty());
}

#[test]
fn terminal_selections_match_provider_compaction_by_turn_and_call() {
    let selected_turn = wire_uuid(1);
    let selected_call = wire_uuid(2);
    let other_turn = wire_uuid(3);
    let other_call = wire_uuid(4);
    let selected_frontier = wire_uuid(5);
    let compaction = |turn_id, model_call_id| SnapshotEntry {
        entry_index: 0,
        source_session_id: wire_uuid(10),
        entry_id: wire_uuid(11),
        kind: SnapshotEntryKind::Marker(TranscriptEntry::ProviderCompaction {
            turn_id,
            model_call_id,
        }),
    };
    let context = super::SnapshotSelectionContext::default();

    for selection in [
        SnapshotSelection::Completed {
            turn_id: selected_turn,
            model_call_id: selected_call,
            terminal_entry_id: wire_uuid(12),
        },
        SnapshotSelection::Refused {
            turn_id: selected_turn,
            model_call_id: selected_call,
            terminal_frontier_id: selected_frontier,
        },
        SnapshotSelection::ToolBatchProposed {
            turn_id: selected_turn,
            model_call_id: selected_call,
        },
        SnapshotSelection::ToolBatchResults {
            turn_id: selected_turn,
            model_call_id: selected_call,
        },
    ] {
        assert!(selection.includes(&compaction(selected_turn, selected_call), &context));
        assert!(!selection.includes(&compaction(other_turn, selected_call), &context));
        assert!(!selection.includes(&compaction(selected_turn, other_call), &context));
    }
}

#[test]
fn terminal_selections_match_provider_reasoning_by_turn_and_call() {
    let selected_turn = wire_uuid(1);
    let selected_call = wire_uuid(2);
    let other_turn = wire_uuid(3);
    let other_call = wire_uuid(4);
    let selected_frontier = wire_uuid(5);
    let reasoning = |turn_id, model_call_id| SnapshotEntry {
        entry_index: 0,
        source_session_id: wire_uuid(10),
        entry_id: wire_uuid(11),
        kind: SnapshotEntryKind::Marker(TranscriptEntry::ProviderReasoning {
            turn_id,
            model_call_id,
        }),
    };
    let context = super::SnapshotSelectionContext::default();

    for selection in [
        SnapshotSelection::Completed {
            turn_id: selected_turn,
            model_call_id: selected_call,
            terminal_entry_id: wire_uuid(12),
        },
        SnapshotSelection::Refused {
            turn_id: selected_turn,
            model_call_id: selected_call,
            terminal_frontier_id: selected_frontier,
        },
        SnapshotSelection::ToolBatchProposed {
            turn_id: selected_turn,
            model_call_id: selected_call,
        },
        SnapshotSelection::ToolBatchResults {
            turn_id: selected_turn,
            model_call_id: selected_call,
        },
    ] {
        assert!(selection.includes(&reasoning(selected_turn, selected_call), &context));
        assert!(!selection.includes(&reasoning(other_turn, selected_call), &context));
        assert!(!selection.includes(&reasoning(selected_turn, other_call), &context));
    }
}

#[test]
fn refused_terminal_reread_renders_its_provider_compaction_marker() {
    let selected_turn = wire_uuid(1);
    let selected_call = wire_uuid(2);
    let other_call = wire_uuid(3);
    let selected_frontier = wire_uuid(4);
    let mut snapshot = TranscriptSnapshot::from_messages(
        12,
        [
            ServerMessage::TranscriptTurn {
                turn_id: selected_turn,
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Refused {
                    terminal_frontier_id: selected_frontier,
                    terminal_attempt_id: wire_uuid(5),
                    terminal_model_call_id: selected_call,
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(11),
                entry: TranscriptEntry::ProviderCompaction {
                    turn_id: selected_turn,
                    model_call_id: selected_call,
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(1),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(12),
                entry: TranscriptEntry::ProviderCompaction {
                    turn_id: selected_turn,
                    model_call_id: other_call,
                },
            },
        ],
    )
    .expect("test snapshot must spool");
    let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .terminal_material(
            &mut snapshot,
            &mut displayed,
            SnapshotSelection::Refused {
                turn_id: selected_turn,
                model_call_id: selected_call,
                terminal_frontier_id: selected_frontier,
            },
        )
        .expect("a refused compaction marker must render without refusal text");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    assert!(rendered.contains(&format!(
        "provider_compaction turn={selected_turn} call={selected_call}"
    )));
    assert!(!rendered.contains(&format!("call={other_call}")));
    assert!(stderr.is_empty());
}

#[test]
fn refused_terminal_reread_requires_its_exact_durable_turn_anchor() {
    let selected_turn = wire_uuid(1);
    let selected_call = wire_uuid(2);
    let selected_frontier = wire_uuid(3);
    let mismatched_anchors = [
        None,
        Some((selected_turn, wire_uuid(4), selected_frontier)),
        Some((selected_turn, selected_call, wire_uuid(5))),
        Some((wire_uuid(6), selected_call, selected_frontier)),
    ];

    for anchor in mismatched_anchors {
        let mut messages = vec![ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: wire_uuid(10),
            entry_id: wire_uuid(11),
            entry: TranscriptEntry::ProviderCompaction {
                turn_id: selected_turn,
                model_call_id: selected_call,
            },
        }];
        if let Some((turn_id, model_call_id, frontier_id)) = anchor {
            messages.insert(
                0,
                ServerMessage::TranscriptTurn {
                    turn_id,
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state: TurnState::Refused {
                        terminal_frontier_id: frontier_id,
                        terminal_attempt_id: wire_uuid(7),
                        terminal_model_call_id: model_call_id,
                    },
                },
            );
        }
        let mut snapshot =
            TranscriptSnapshot::from_messages(12, messages).expect("snapshot must spool");
        let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = Output::new(&mut stdout, &mut stderr, false)
            .terminal_material(
                &mut snapshot,
                &mut displayed,
                SnapshotSelection::Refused {
                    turn_id: selected_turn,
                    model_call_id: selected_call,
                    terminal_frontier_id: selected_frontier,
                },
            )
            .expect_err("a refused reread must require the event's exact turn anchor");

        assert!(matches!(
            error,
            ClientError::Protocol("terminal reread omitted the event's exact marker")
        ));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }
}

#[test]
fn tool_reconciliation_reread_uses_its_terminal_turn_batch() {
    let selected_turn = wire_uuid(1);
    let selected_call = wire_uuid(2);
    let selected_request = wire_uuid(3);
    let selected_attempt = wire_uuid(4);
    let selected_frontier = wire_uuid(5);
    let later_turn = wire_uuid(6);
    let later_call = wire_uuid(7);
    let later_request = wire_uuid(8);
    let mut snapshot = TranscriptSnapshot::from_messages(
        12,
        [
            ServerMessage::TranscriptTurn {
                turn_id: selected_turn,
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::ToolReconciliationRequired {
                    terminal_frontier_id: selected_frontier,
                    terminal_attempt_id: wire_uuid(9),
                    terminal_tool_attempt_id: selected_attempt,
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(11),
                entry: TranscriptEntry::AssistantToolUse {
                    turn_id: selected_turn,
                    model_call_id: selected_call,
                    tool_request_id: selected_request,
                    tool_name: String::from("selected"),
                    arguments: String::from("{}"),
                    approval: None,
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(1),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(12),
                entry: TranscriptEntry::ToolClosed {
                    tool_request_id: selected_request,
                    content: String::from("selected result"),
                    approved_before_close: false,
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(2),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(13),
                entry: TranscriptEntry::AssistantToolUse {
                    turn_id: later_turn,
                    model_call_id: later_call,
                    tool_request_id: later_request,
                    tool_name: String::from("later"),
                    arguments: String::from("{}"),
                    approval: None,
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(3),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(14),
                entry: TranscriptEntry::ToolClosed {
                    tool_request_id: later_request,
                    content: String::from("later result"),
                    approved_before_close: false,
                },
            },
        ],
    )
    .expect("test snapshot must spool");
    let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .terminal_material(
            &mut snapshot,
            &mut displayed,
            SnapshotSelection::ToolReconciliation {
                turn_id: selected_turn,
                tool_attempt_id: selected_attempt,
                terminal_frontier_id: selected_frontier,
            },
        )
        .expect("the exact terminal tool batch renders");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    assert!(rendered.contains("selected result"));
    assert!(!rendered.contains("later result"));
    assert!(stderr.is_empty());
}

#[test]
fn terminal_reread_rejects_a_missing_exact_marker_before_output() {
    let selected_turn = wire_uuid(1);
    let selected_call = wire_uuid(2);
    let mut snapshot = TranscriptSnapshot::from_messages(
        12,
        [
            ServerMessage::TranscriptTextEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(11),
                entry: TranscriptTextEntry::Assistant {
                    turn_id: selected_turn,
                    model_call_id: selected_call,
                },
            },
            ServerMessage::TranscriptContent {
                entry_index: CanonicalU64::new(0),
                fragment_index: CanonicalU64::new(0),
                final_fragment: true,
                content_fragment: ContentFragment::try_new("untrusted reply".to_owned())
                    .expect("short content is valid"),
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(1),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(12),
                entry: TranscriptEntry::TurnCompleted {
                    turn_id: selected_turn,
                },
            },
        ],
    )
    .expect("test snapshot must spool");
    let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let error = Output::new(&mut stdout, &mut stderr, false)
        .terminal_material(
            &mut snapshot,
            &mut displayed,
            SnapshotSelection::Completed {
                turn_id: selected_turn,
                model_call_id: selected_call,
                terminal_entry_id: wire_uuid(13),
            },
        )
        .expect_err("a side reread without the event marker must fail closed");

    assert!(matches!(
        error,
        ClientError::Protocol("terminal reread omitted the event's exact marker")
    ));
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn failed_terminal_reread_rejects_a_different_marker_identity() {
    let selected_turn = wire_uuid(1);
    let mut snapshot = TranscriptSnapshot::from_messages(
        12,
        [ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: wire_uuid(10),
            entry_id: wire_uuid(11),
            entry: TranscriptEntry::TurnFailed {
                turn_id: selected_turn,
            },
        }],
    )
    .expect("test snapshot must spool");
    let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let error = Output::new(&mut stdout, &mut stderr, false)
        .terminal_material(
            &mut snapshot,
            &mut displayed,
            SnapshotSelection::Failed {
                turn_id: selected_turn,
                terminal_entry_id: wire_uuid(12),
            },
        )
        .expect_err("a failed reread must require the event marker");

    assert!(matches!(
        error,
        ClientError::Protocol("terminal reread omitted the event's exact marker")
    ));
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn snapshot_renders_cancellation_requested_call() {
    let rendered = render_snapshot_turn(TurnState::ActiveRunning {
        current_attempt_id: wire_uuid(2),
        current_model_call: Some(CurrentModelCall::new(
            wire_uuid(3),
            CurrentModelCallState::CancellationRequested {},
        )),
    });

    expect![[r#"
        turn=00000000-0000-0000-0000-000000000001 position=1 state=active_running attempt=00000000-0000-0000-0000-000000000002 call=00000000-0000-0000-0000-000000000003 call_state=cancellation_requested
        usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn snapshot_renders_queued_delegated_origin() {
    let rendered = render_snapshot_turn(TurnState::QueuedDelegated {
        spawning_request_id: wire_uuid(2),
        parent_session_id: wire_uuid(3),
        parent_turn_id: wire_uuid(4),
        content: InputContent::new(String::from("delegated task")),
    });

    expect![[r#"
        turn=00000000-0000-0000-0000-000000000001 position=1 state=queued_delegated spawning_request=00000000-0000-0000-0000-000000000002 parent_session=00000000-0000-0000-0000-000000000003 parent_turn=00000000-0000-0000-0000-000000000004
        delegated task
        usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn snapshot_renders_queued_delegation_wake_range() {
    let rendered = render_snapshot_turn(TurnState::QueuedDelegationWake {
        first_delivery_sequence: CanonicalU64::new(3),
        through_delivery_sequence: CanonicalU64::new(5),
    });

    expect![[r#"
        turn=00000000-0000-0000-0000-000000000001 position=1 state=queued_delegation_wake deliveries=3-5
        usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn snapshot_renders_failed_call_evidence() {
    let rendered = render_snapshot_turn(TurnState::Failed {
        terminal_frontier_id: wire_uuid(2),
        terminal_attempt_id: Some(wire_uuid(3)),
        terminal_model_call: Some(FailedTerminalModelCall::new(
            wire_uuid(4),
            FailedModelCallDisposition::Cancelled,
        )),
    });

    expect![[r#"
        turn=00000000-0000-0000-0000-000000000001 position=1 state=failed frontier=00000000-0000-0000-0000-000000000002 attempt=00000000-0000-0000-0000-000000000003 call=00000000-0000-0000-0000-000000000004 call_disposition=cancelled call_cause=none
        usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn snapshot_renders_cancelled_turn() {
    let rendered = render_snapshot_turn(TurnState::Cancelled {
        terminal_frontier_id: wire_uuid(2),
        terminal_attempt_id: wire_uuid(3),
        terminal_model_call_id: None,
    });

    expect![[r#"
        turn=00000000-0000-0000-0000-000000000001 position=1 state=cancelled frontier=00000000-0000-0000-0000-000000000002 attempt=00000000-0000-0000-0000-000000000003 call=none
        usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn snapshot_renders_reconciliation_required_turn() {
    let rendered = render_snapshot_turn(TurnState::ReconciliationRequired {
        terminal_frontier_id: wire_uuid(2),
        terminal_attempt_id: wire_uuid(3),
        terminal_model_call_id: wire_uuid(4),
    });

    expect![[r#"
        turn=00000000-0000-0000-0000-000000000001 position=1 state=reconciliation_required frontier=00000000-0000-0000-0000-000000000002 attempt=00000000-0000-0000-0000-000000000003 operation=model_call operation_id=00000000-0000-0000-0000-000000000004
        usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn transcript_without_terminal_calls_renders_a_session_usage_total() {
    let mut snapshot = TranscriptSnapshot::from_messages(1, std::iter::empty::<ServerMessage>())
        .expect("test snapshot must spool");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .snapshot(&mut snapshot)
        .expect("empty usage snapshot must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        usage_total scope=session usage_provenance=reported terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn transcript_usage_preserves_zero_absence_and_partial_coverage() {
    let first_turn = wire_uuid(1);
    let second_turn = wire_uuid(2);
    let mut snapshot = TranscriptSnapshot::from_messages(
        1,
        [
            ServerMessage::TranscriptModelCallUsage {
                model_call_index: CanonicalU64::new(0),
                turn_id: first_turn,
                model_call_id: wire_uuid(11),
                usage_provenance: UsageProvenance::Reported,
                usage: ModelCallTokenUsage {
                    input_tokens: Some(CanonicalU64::new(10)),
                    output_tokens: Some(CanonicalU64::new(0)),
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: Some(CanonicalU64::new(4)),
                },
                cost: None,
            },
            ServerMessage::TranscriptModelCallUsage {
                model_call_index: CanonicalU64::new(1),
                turn_id: first_turn,
                model_call_id: wire_uuid(12),
                usage_provenance: UsageProvenance::Reported,
                usage: ModelCallTokenUsage {
                    input_tokens: None,
                    output_tokens: None,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
                cost: None,
            },
            ServerMessage::TranscriptModelCallUsage {
                model_call_index: CanonicalU64::new(2),
                turn_id: second_turn,
                model_call_id: wire_uuid(13),
                usage_provenance: UsageProvenance::Reported,
                usage: ModelCallTokenUsage {
                    input_tokens: None,
                    output_tokens: None,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
                cost: None,
            },
        ],
    )
    .expect("test snapshot must spool");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .snapshot(&mut snapshot)
        .expect("usage snapshot must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        usage turn=00000000-0000-0000-0000-000000000001 usage_provenance=reported terminal_calls=2 input_tokens=10 input_tokens_present_calls=1/2 output_tokens=0 output_tokens_present_calls=1/2 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/2 cache_read_input_tokens=4 cache_read_input_tokens_present_calls=1/2
        usage turn=00000000-0000-0000-0000-000000000001 usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage turn=00000000-0000-0000-0000-000000000002 usage_provenance=reported terminal_calls=1 input_tokens=unreported input_tokens_present_calls=0/1 output_tokens=unreported output_tokens_present_calls=0/1 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/1 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/1
        usage turn=00000000-0000-0000-0000-000000000002 usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
        usage_total scope=session usage_provenance=reported terminal_calls=3 input_tokens=10 input_tokens_present_calls=1/3 output_tokens=0 output_tokens_present_calls=1/3 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/3 cache_read_input_tokens=4 cache_read_input_tokens_present_calls=1/3
        usage_total scope=session usage_provenance=estimated terminal_calls=0 input_tokens=unreported input_tokens_present_calls=0/0 output_tokens=unreported output_tokens_present_calls=0/0 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/0 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/0
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn transcript_costs_aggregate_only_with_matching_provenance_and_labels() {
    let turn = wire_uuid(1);
    let usage = ModelCallTokenUsage {
        input_tokens: Some(CanonicalU64::new(0)),
        output_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    let mut snapshot = TranscriptSnapshot::from_messages(
        1,
        [
            ServerMessage::TranscriptModelCallUsage {
                model_call_index: CanonicalU64::new(0),
                turn_id: turn,
                model_call_id: wire_uuid(11),
                usage_provenance: UsageProvenance::Reported,
                usage,
                cost: Some(ModelCallDollarCost {
                    amount_usd: CanonicalDollarAmount::try_new(String::from("0.1"))
                        .expect("fixture dollar amount is canonical"),
                    rate_version: BillingRateVersion::try_new(String::from("rates-v1"))
                        .expect("fixture rate version is valid"),
                    label: ModelCallCostLabel::Real,
                }),
            },
            ServerMessage::TranscriptModelCallUsage {
                model_call_index: CanonicalU64::new(1),
                turn_id: turn,
                model_call_id: wire_uuid(12),
                usage_provenance: UsageProvenance::Reported,
                usage,
                cost: Some(ModelCallDollarCost {
                    amount_usd: CanonicalDollarAmount::try_new(String::from("0.2"))
                        .expect("fixture dollar amount is canonical"),
                    rate_version: BillingRateVersion::try_new(String::from("rates-v1"))
                        .expect("fixture rate version is valid"),
                    label: ModelCallCostLabel::Real,
                }),
            },
            ServerMessage::TranscriptModelCallUsage {
                model_call_index: CanonicalU64::new(2),
                turn_id: turn,
                model_call_id: wire_uuid(13),
                usage_provenance: UsageProvenance::Estimated,
                usage,
                cost: Some(ModelCallDollarCost {
                    amount_usd: CanonicalDollarAmount::try_new(String::from("0.4"))
                        .expect("fixture dollar amount is canonical"),
                    rate_version: BillingRateVersion::try_new(String::from("rates-v1"))
                        .expect("fixture rate version is valid"),
                    label: ModelCallCostLabel::MeteredEquivalent,
                }),
            },
        ],
    )
    .expect("test snapshot must spool");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .snapshot(&mut snapshot)
        .expect("cost snapshot must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    expect![[r#"
        usage turn=00000000-0000-0000-0000-000000000001 usage_provenance=reported terminal_calls=2 input_tokens=0 input_tokens_present_calls=2/2 output_tokens=unreported output_tokens_present_calls=0/2 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/2 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/2
        usage turn=00000000-0000-0000-0000-000000000001 usage_provenance=estimated terminal_calls=1 input_tokens=0 input_tokens_present_calls=1/1 output_tokens=unreported output_tokens_present_calls=0/1 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/1 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/1
        cost turn=00000000-0000-0000-0000-000000000001 usage_provenance=reported label=real rate_version=rates-v1 usd=0.3 costed_calls=2
        cost turn=00000000-0000-0000-0000-000000000001 usage_provenance=estimated label=metered_equivalent rate_version=rates-v1 usd=0.4 costed_calls=1
        usage_total scope=session usage_provenance=reported terminal_calls=2 input_tokens=0 input_tokens_present_calls=2/2 output_tokens=unreported output_tokens_present_calls=0/2 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/2 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/2
        usage_total scope=session usage_provenance=estimated terminal_calls=1 input_tokens=0 input_tokens_present_calls=1/1 output_tokens=unreported output_tokens_present_calls=0/1 cache_creation_input_tokens=unreported cache_creation_input_tokens_present_calls=0/1 cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/1
        cost_total scope=session usage_provenance=reported label=real rate_version=rates-v1 usd=0.3 costed_calls=2
        cost_total scope=session usage_provenance=estimated label=metered_equivalent rate_version=rates-v1 usd=0.4 costed_calls=1
    "#]]
    .assert_eq(&rendered);
    assert!(stderr.is_empty());
}

#[test]
fn raw_transcript_cost_rate_version_is_unchanged() {
    let rate_version = String::from("rates v1");
    let mut snapshot = TranscriptSnapshot::from_messages(
        1,
        [ServerMessage::TranscriptModelCallUsage {
            model_call_index: CanonicalU64::new(0),
            turn_id: wire_uuid(1),
            model_call_id: wire_uuid(11),
            usage_provenance: UsageProvenance::Reported,
            usage: ModelCallTokenUsage {
                input_tokens: Some(CanonicalU64::new(0)),
                output_tokens: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
            cost: Some(ModelCallDollarCost {
                amount_usd: CanonicalDollarAmount::try_new(String::from("0.1"))
                    .expect("fixture dollar amount is canonical"),
                rate_version: BillingRateVersion::try_new(rate_version.clone())
                    .expect("fixture rate version is valid"),
                label: ModelCallCostLabel::Real,
            }),
        }],
    )
    .expect("test snapshot must spool");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, true)
        .snapshot(&mut snapshot)
        .expect("raw cost snapshot must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    assert!(rendered.contains(&format!("rate_version={rate_version} ")));
    assert!(stderr.is_empty());
}

#[test]
fn transcript_cost_totals_grow_on_disk_and_retain_values() {
    let later = CostAggregateKey {
        provenance: UsageProvenance::Reported,
        label: ModelCallCostLabel::Real,
        rate_version: String::from("rates-z"),
    };
    let earlier = CostAggregateKey {
        provenance: UsageProvenance::Reported,
        label: ModelCallCostLabel::Real,
        rate_version: String::from("rates-a"),
    };
    let mut totals = DiskCostTotals::with_capacity(2).expect("the test cost spool must open");
    totals
        .add(&later, Decimal::new(2, 1))
        .expect("the later key must spool");
    totals
        .add(&earlier, Decimal::new(1, 1))
        .expect("the earlier key must grow and spool");

    let later_total = totals
        .get(&later)
        .expect("the cost spool must read")
        .expect("the later key must exist");
    let earlier_total = totals
        .get(&earlier)
        .expect("the cost spool must read")
        .expect("the earlier key must exist");

    assert_eq!(totals.capacity, 4);
    assert_eq!(later_total.amount_usd, Decimal::new(2, 1));
    assert_eq!(later_total.calls, 1);
    assert_eq!(earlier_total.amount_usd, Decimal::new(1, 1));
    assert_eq!(earlier_total.calls, 1);
}

#[test]
fn transcript_cost_totals_reject_inexact_decimal_addition() {
    let key = CostAggregateKey {
        provenance: UsageProvenance::Reported,
        label: ModelCallCostLabel::Real,
        rate_version: String::from("rates-v1"),
    };
    let large = Decimal::from_str("10000000000000000000000000000")
        .expect("fixture dollar amount is representable");
    let tiny = Decimal::from_str("0.0000000000000000000000000001")
        .expect("fixture dollar amount is representable");
    let mut totals = DiskCostTotals::with_capacity(2).expect("the test cost spool must open");
    totals
        .add(&key, large)
        .expect("the first exact amount must spool");

    let error = totals
        .add(&key, tiny)
        .expect_err("an inexact aggregate must be rejected");
    let retained = totals
        .get(&key)
        .expect("the cost spool must read")
        .expect("the original total must remain");

    assert!(matches!(
        error,
        ClientError::Protocol("dollar cost total was inexact")
    ));
    assert_eq!(retained.amount_usd, large);
    assert_eq!(retained.calls, 1);
}

#[test]
fn transcript_is_not_partially_published_when_a_later_total_is_inexact() {
    let large = Decimal::from_str("10000000000000000000000000000")
        .expect("fixture dollar amount is representable");
    let tiny = Decimal::from_str("0.0000000000000000000000000001")
        .expect("fixture dollar amount is representable");
    let rate_version = BillingRateVersion::try_new(String::from("rates-v1"))
        .expect("fixture rate version is valid");
    let usage = ModelCallTokenUsage {
        input_tokens: Some(CanonicalU64::new(0)),
        output_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    let mut snapshot = TranscriptSnapshot::from_messages(
        1,
        [
            ServerMessage::TranscriptTurn {
                turn_id: wire_uuid(1),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Queued {
                    accepted_input_id: wire_uuid(10),
                    content: UserInputContent::text("transcript content".to_owned()),
                },
            },
            ServerMessage::TranscriptModelCallUsage {
                model_call_index: CanonicalU64::new(0),
                turn_id: wire_uuid(1),
                model_call_id: wire_uuid(11),
                usage_provenance: UsageProvenance::Reported,
                usage,
                cost: Some(ModelCallDollarCost {
                    amount_usd: CanonicalDollarAmount::try_new(large.to_string())
                        .expect("fixture dollar amount is canonical"),
                    rate_version: rate_version.clone(),
                    label: ModelCallCostLabel::Real,
                }),
            },
            ServerMessage::TranscriptModelCallUsage {
                model_call_index: CanonicalU64::new(1),
                turn_id: wire_uuid(2),
                model_call_id: wire_uuid(12),
                usage_provenance: UsageProvenance::Reported,
                usage,
                cost: Some(ModelCallDollarCost {
                    amount_usd: CanonicalDollarAmount::try_new(tiny.to_string())
                        .expect("fixture dollar amount is canonical"),
                    rate_version,
                    label: ModelCallCostLabel::Real,
                }),
            },
        ],
    )
    .expect("test snapshot must spool");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let error = Output::new(&mut stdout, &mut stderr, false)
        .snapshot(&mut snapshot)
        .expect_err("the inexact session total must be rejected");

    assert!(matches!(
        error,
        ClientError::Protocol("dollar cost total was inexact")
    ));
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn follow_event_renders_cancellation_requested_call() {
    let rendered = render_event(SessionEvent::ModelCallTransition {
        turn_id: wire_uuid(2),
        model_call_id: wire_uuid(3),
        state: ModelCallState::CancellationRequested {},
    });

    expect![[r#"
        event=1 session=00000000-0000-0000-0000-000000000001 model_call_transition turn=00000000-0000-0000-0000-000000000002 call=00000000-0000-0000-0000-000000000003 state=cancellation_requested
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn follow_event_renders_runner_working_directory_change() {
    let rendered = render_event(SessionEvent::RunnerStateTransition {
        runner_id: wire_uuid(2),
        placement_revision: RunnerPlacementRevision::try_new(3)
            .expect("the fixture placement revision is positive"),
        sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
        working_directory: Some(
            RunnerWorkingDirectory::try_new(String::from("workspace root\nproject"))
                .expect("the fixture working directory is valid"),
        ),
        state: RunnerStateTransitionState::WorkingDirectoryChanged,
    });

    expect![[r#"
        event=1 session=00000000-0000-0000-0000-000000000001 runner_state_transition runner=00000000-0000-0000-0000-000000000002 placement_revision=3 sandbox=workspace_restricted working_directory=workspace\u{20}root\u{a}project state=working_directory_changed
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn follow_event_distinguishes_default_from_literal_none_working_directory() {
    let default_directory = render_event(SessionEvent::RunnerStateTransition {
        runner_id: wire_uuid(2),
        placement_revision: RunnerPlacementRevision::try_new(3)
            .expect("the fixture placement revision is positive"),
        sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
        working_directory: None,
        state: RunnerStateTransitionState::Pinned,
    });
    let literal_none = render_event(SessionEvent::RunnerStateTransition {
        runner_id: wire_uuid(2),
        placement_revision: RunnerPlacementRevision::try_new(3)
            .expect("the fixture placement revision is positive"),
        sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
        working_directory: Some(
            RunnerWorkingDirectory::try_new(String::from("none"))
                .expect("the fixture working directory is valid"),
        ),
        state: RunnerStateTransitionState::Pinned,
    });

    expect![[r#"
        event=1 session=00000000-0000-0000-0000-000000000001 runner_state_transition runner=00000000-0000-0000-0000-000000000002 placement_revision=3 sandbox=workspace_restricted state=pinned
        event=1 session=00000000-0000-0000-0000-000000000001 runner_state_transition runner=00000000-0000-0000-0000-000000000002 placement_revision=3 sandbox=workspace_restricted working_directory=none state=pinned
    "#]]
    .assert_eq(&format!("{default_directory}{literal_none}"));
}

#[test]
fn follow_event_renders_delegate_tool_decision_and_rationale() {
    let rendered = render_event(SessionEvent::ToolApprovalDecided {
        turn_id: wire_uuid(2),
        tool_request_id: wire_uuid(3),
        decision: ToolApprovalEventDecision::Deny { reason: None },
        decider: ToolApprovalEventDecider::Delegate {
            model_selection_id: wire_uuid(4),
            model_call_id: wire_uuid(5),
        },
        rationale: Some(String::from(
            "request exceeds configured authority\nreview manually",
        )),
    });

    expect![[r#"
        event=1 session=00000000-0000-0000-0000-000000000001 tool_approval_decided turn=00000000-0000-0000-0000-000000000002 request=00000000-0000-0000-0000-000000000003 decision=deny decider=delegate model_selection=00000000-0000-0000-0000-000000000004 call=00000000-0000-0000-0000-000000000005
        rationale=request exceeds configured authority\u{a}review manually
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn follow_event_renders_user_tool_denial_and_reason() {
    let rendered = render_event(SessionEvent::ToolApprovalDecided {
        turn_id: wire_uuid(2),
        tool_request_id: wire_uuid(3),
        decision: ToolApprovalEventDecision::Deny {
            reason: Some(String::from("outside requested scope")),
        },
        decider: ToolApprovalEventDecider::User {
            command_id: wire_uuid(4),
        },
        rationale: None,
    });

    expect![[r#"
        event=1 session=00000000-0000-0000-0000-000000000001 tool_approval_decided turn=00000000-0000-0000-0000-000000000002 request=00000000-0000-0000-0000-000000000003 decision=deny decider=user command=00000000-0000-0000-0000-000000000004
        denial_reason=outside requested scope
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn follow_event_renders_cancelled_turn() {
    let rendered = render_event(SessionEvent::TurnCancelled {
        turn_id: wire_uuid(2),
        cancellation_entry_id: wire_uuid(3),
        terminal_frontier_id: wire_uuid(4),
    });

    expect![[r#"
        event=1 session=00000000-0000-0000-0000-000000000001 turn_cancelled turn=00000000-0000-0000-0000-000000000002 entry=00000000-0000-0000-0000-000000000003 frontier=00000000-0000-0000-0000-000000000004
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn follow_event_renders_reconciliation_required_turn() {
    let rendered = render_event(SessionEvent::TurnReconciliationRequired {
        turn_id: wire_uuid(2),
        model_call_id: wire_uuid(3),
        terminal_frontier_id: wire_uuid(4),
    });

    expect![[r#"
        event=1 session=00000000-0000-0000-0000-000000000001 turn_reconciliation_required turn=00000000-0000-0000-0000-000000000002 operation=model_call operation_id=00000000-0000-0000-0000-000000000003 frontier=00000000-0000-0000-0000-000000000004
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn follow_event_renders_bound_child_policy() {
    let rendered = render_event(SessionEvent::ChildSpawned {
        spawning_request_id: wire_uuid(2),
        child_session_id: wire_uuid(3),
        relationship: DelegationPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        },
    });

    expect![[r#"
        event=1 session=00000000-0000-0000-0000-000000000001 delegation_child_spawned spawning_request=00000000-0000-0000-0000-000000000002 child=00000000-0000-0000-0000-000000000003 policy=bound on_parent_stopped=stop on_parent_cancelled=cancel
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn follow_event_renders_returned_child_result_content() {
    let rendered = render_event(SessionEvent::ChildResult {
        spawning_request_id: wire_uuid(2),
        child_session_id: wire_uuid(3),
        outcome: DelegationOutcome::Returned,
        content: Some(String::from("delivered result")),
        reason: DelegationReason::ChildCompleted,
        provenance: DelegationProvenance::ChildTurn {
            child_session_id: wire_uuid(3),
            child_turn_id: wire_uuid(4),
        },
    });

    expect![[r#"
        event=1 session=00000000-0000-0000-0000-000000000001 delegation_child_result spawning_request=00000000-0000-0000-0000-000000000002 child=00000000-0000-0000-0000-000000000003 outcome=returned reason=child_completed provenance=child_turn:00000000-0000-0000-0000-000000000003:00000000-0000-0000-0000-000000000004 content_present=true
        delivered result
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn follow_event_renders_parent_cascade_child_result_without_content() {
    let rendered = render_event(SessionEvent::ChildResult {
        spawning_request_id: wire_uuid(2),
        child_session_id: wire_uuid(3),
        outcome: DelegationOutcome::Stopped,
        content: None,
        reason: DelegationReason::ParentStopped,
        provenance: DelegationProvenance::ParentTurnCommand {
            parent_session_id: wire_uuid(1),
            parent_turn_id: wire_uuid(4),
            command_id: wire_uuid(5),
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        },
    });

    expect![[r#"
        event=1 session=00000000-0000-0000-0000-000000000001 delegation_child_result spawning_request=00000000-0000-0000-0000-000000000002 child=00000000-0000-0000-0000-000000000003 outcome=stopped reason=parent_stopped provenance=parent_turn_command:00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000004:00000000-0000-0000-0000-000000000005:parent_and_descendants content_present=false
    "#]]
    .assert_eq(&rendered);
}

#[test]
fn cancelled_terminal_reread_includes_the_producing_calls_compaction_marker() {
    let selected_turn = wire_uuid(1);
    let selected_call = wire_uuid(2);
    let other_call = wire_uuid(3);
    let later_turn = wire_uuid(4);
    let mut snapshot = TranscriptSnapshot::from_messages(
        12,
        [
            ServerMessage::TranscriptTurn {
                turn_id: selected_turn,
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Cancelled {
                    terminal_frontier_id: wire_uuid(5),
                    terminal_attempt_id: wire_uuid(6),
                    terminal_model_call_id: Some(selected_call),
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(11),
                entry: TranscriptEntry::ProviderCompaction {
                    turn_id: selected_turn,
                    model_call_id: selected_call,
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(1),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(12),
                entry: TranscriptEntry::ProviderCompaction {
                    turn_id: selected_turn,
                    model_call_id: other_call,
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(2),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(13),
                entry: TranscriptEntry::TurnCancelled {
                    turn_id: selected_turn,
                },
            },
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(3),
                source_session_id: wire_uuid(10),
                entry_id: wire_uuid(14),
                entry: TranscriptEntry::TurnCancelled {
                    turn_id: later_turn,
                },
            },
        ],
    )
    .expect("test snapshot must spool");
    let mut displayed = SnapshotIdentitySet::new().expect("identity spool must open");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .terminal_material(
            &mut snapshot,
            &mut displayed,
            SnapshotSelection::Cancelled {
                turn_id: selected_turn,
                terminal_entry_id: wire_uuid(13),
            },
        )
        .expect("selected cancellation marker must render");

    let rendered = String::from_utf8(stdout).expect("rendered output is UTF-8");
    assert!(rendered.contains(&format!(
        "provider_compaction turn={selected_turn} call={selected_call}"
    )));
    assert!(!rendered.contains(&format!("call={other_call}")));
    assert!(rendered.contains("turn_cancelled"));
    assert!(!rendered.contains(&later_turn.to_string()));
    assert!(stderr.is_empty());
}

#[derive(Default)]
struct FlushWriter {
    bytes: Vec<u8>,
    flushes: usize,
}

impl Write for FlushWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

#[track_caller]
fn render_snapshot_turn(state: TurnState) -> String {
    let mut snapshot = TranscriptSnapshot::from_messages(
        1,
        [ServerMessage::TranscriptTurn {
            turn_id: wire_uuid(1),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state,
        }],
    )
    .expect("test snapshot must spool");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .snapshot(&mut snapshot)
        .expect("snapshot turn must render");
    assert!(stderr.is_empty());
    String::from_utf8(stdout).expect("rendered output is UTF-8")
}

#[track_caller]
fn render_event(event: SessionEvent) -> String {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    Output::new(&mut stdout, &mut stderr, false)
        .event(1, wire_uuid(1), &event)
        .expect("event must render");
    assert!(stderr.is_empty());
    String::from_utf8(stdout).expect("rendered output is UTF-8")
}

fn wire_uuid(value: u128) -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::from_u128(value))
}
fn review_target_snapshot(base_revision: Option<String>) -> ReviewTargetSnapshot {
    ReviewTargetSnapshot {
        target_id: wire_uuid(1),
        provider: String::from("example-host"),
        repository: String::from("example/repository"),
        subject: ReviewTargetSubject::Commit {},
        head_revision: String::from("head"),
        base_revision,
        stack_parent_target_id: None,
    }
}

fn review_finding_snapshot() -> ReviewFindingSnapshot {
    ReviewFindingSnapshot {
        target_id: wire_uuid(1),
        run_id: wire_uuid(2),
        producing_pass_id: wire_uuid(3),
        finding: ReviewFindingInput {
            finding_id: wire_uuid(4),
            file_path: String::from("src/lib.rs"),
            line_start: Some(CanonicalU64::new(7)),
            line_end: Some(CanonicalU64::new(9)),
            diff_side: Some(ReviewDiffSide::Right),
            title: String::from("Retain evidence"),
            body: String::from("First line\nSecond line"),
            severity: ReviewSeverity::High,
            is_real_confidence: CanonicalU64::new(9_000),
            severity_label_confidence: CanonicalU64::new(8_500),
            category: String::from("correctness"),
            recommended_fix: Some(String::from("Bind the exact\npass.")),
        },
        status: ReviewFindingStatus::Open,
        event_count: CanonicalU64::new(2),
    }
}
