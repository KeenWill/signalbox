//! Server message family protocol tests.

use super::support::*;
use crate::*;

#[test]
fn server_message_family_has_exact_closed_wire_shapes() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::SessionCreated {
            session_id: uuid(1),
            model_settings: provider_default_settings_snapshot_fixture(),
        },
        &format!(
            "{{\"type\":\"session_created\",\"session_id\":\"00000000-0000-0000-0000-000000000001\",\"model_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON}}}"
        ),
    )?;
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::InputSubmitted {
            termination: None,
            session_id: uuid(1),
            accepted_input_id: uuid(2),
            acceptance_position: CanonicalU64::new(1),
            turn_id: uuid(3),
            model_settings: settings_snapshot_fixture(),
        },
        &format!(
            "{{\"type\":\"input_submitted\",\"session_id\":\"00000000-0000-0000-0000-000000000001\",\"accepted_input_id\":\"00000000-0000-0000-0000-000000000002\",\"acceptance_position\":\"1\",\"turn_id\":\"00000000-0000-0000-0000-000000000003\",\"model_settings\":{SETTINGS_SNAPSHOT_JSON}}}"
        ),
    )?;
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::SessionsStart {},
        r#"{"type":"sessions_start"}"#,
    )?;
    assert_server_message_round_trip(
        request(4)?,
        ServerMessage::SessionSummary {
            session_id: uuid(1),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Alias { alias_id: uuid(4) },
            placement_version: CanonicalU64::new(1),
            placement: crate::SessionPlacement::Pathless {},
            runner: None,
        },
        r#"{"type":"session_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{"kind":"alias","alias_id":"00000000-0000-0000-0000-000000000004"},"placement_version":"1","placement":{"kind":"pathless"},"runner":null}"#,
    )?;
    assert_server_message_round_trip(
        request(5)?,
        ServerMessage::SessionsEnd {
            session_count: CanonicalU64::new(1),
        },
        r#"{"type":"sessions_end","session_count":"1"}"#,
    )?;
    let writer = MetadataLastWriter::new(CanonicalU64::new(17), MetadataActor::User {});
    assert_server_message_round_trip(
        request(32)?,
        ServerMessage::SessionMetadataPageStart {},
        r#"{"type":"session_metadata_page_start"}"#,
    )?;
    assert_server_message_round_trip(
        request(33)?,
        ServerMessage::SessionMetadataSummary {
            session_id: uuid(1),
            defaults_version: CanonicalU64::new(2),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            dangerous_tool_auto_approval: false,
            title: Some(String::from("Planning")),
            tags: vec![String::from("daily"), String::from("work")],
            archived: true,
            last_writer: Some(writer),
        },
        r#"{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"2","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":false,"title":"Planning","tags":["daily","work"],"archived":true,"last_writer":{"updated_at_unix_micros":"17","actor":{"type":"user"}}}"#,
    )?;
    assert_server_message_round_trip(
        request(34)?,
        ServerMessage::SessionMetadataPageEnd {
            session_count: CanonicalU64::new(1),
            next_after_session_id: Some(uuid(1)),
        },
        r#"{"type":"session_metadata_page_end","session_count":"1","next_after_session_id":"00000000-0000-0000-0000-000000000001"}"#,
    )?;
    assert_server_message_round_trip(
        request(35)?,
        ServerMessage::SessionMetadata {
            session_id: uuid(1),
            metadata: SessionMetadata::empty(),
            last_writer: None,
        },
        r#"{"type":"session_metadata","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false},"last_writer":null}"#,
    )?;
    assert_server_message_round_trip(
        request(36)?,
        ServerMessage::SessionMetadataReplaced {
            session_id: uuid(1),
            metadata: metadata(true)?,
            last_writer: writer,
        },
        r#"{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":"Planning","tags":["daily","work"],"attributes":{"run":"17","trigger":""},"archived":true},"last_writer":{"updated_at_unix_micros":"17","actor":{"type":"user"}}}"#,
    )?;
    assert_server_message_round_trip(
        request(6)?,
        ServerMessage::TranscriptSnapshotStart {
            workspace_root_kind: None,
            repository_watch: None,
            session_id: uuid(1),
            cursor: CanonicalU64::new(5),
            runner: None,
        },
        r#"{"type":"transcript_snapshot_start","workspace_root_kind":null,"repository_watch":null,"session_id":"00000000-0000-0000-0000-000000000001","cursor":"5","runner":null}"#,
    )?;
    assert_server_message_round_trip(
        request(7)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Refused {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: uuid(7),
                terminal_model_call_id: uuid(8),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"refused","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
    )?;
    assert_server_message_round_trip(
        request(14)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Queued {
                accepted_input_id: uuid(2),
                content: UserInputContent::text("queued request".to_owned()),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"queued","accepted_input_id":"00000000-0000-0000-0000-000000000002","content":[{"type":"text","text":"queued request"}]}}"#,
    )?;
    assert_server_message_round_trip(
        request(15)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::ActiveRunning {
                current_attempt_id: uuid(7),
                current_model_call: None,
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":null}}"#,
    )?;
    assert_server_message_round_trip(
        request(16)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::ActiveRunning {
                current_attempt_id: uuid(7),
                current_model_call: Some(CurrentModelCall::new(
                    uuid(8),
                    CurrentModelCallState::Prepared {},
                )),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"prepared"}}}}"#,
    )?;
    assert_server_message_round_trip(
        request(17)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::ActiveRunning {
                current_attempt_id: uuid(7),
                current_model_call: Some(CurrentModelCall::new(
                    uuid(8),
                    CurrentModelCallState::InFlight {},
                )),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"in_flight"}}}}"#,
    )?;
    assert_server_message_round_trip(
        request(20)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::ActiveRunning {
                current_attempt_id: uuid(7),
                current_model_call: Some(CurrentModelCall::new(
                    uuid(8),
                    CurrentModelCallState::CancellationRequested {},
                )),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"cancellation_requested"}}}}"#,
    )?;
    assert_server_message_round_trip(
        request(21)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Failed {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: None,
                terminal_model_call: None,
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":null,"terminal_model_call":null}}"#,
    )?;
    assert_server_message_round_trip(
        request(22)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Failed {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: Some(uuid(7)),
                terminal_model_call: None,
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call":null}}"#,
    )?;
    assert_server_message_round_trip(
        request(23)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Failed {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: Some(uuid(7)),
                terminal_model_call: Some(FailedTerminalModelCall::new(
                    uuid(8),
                    FailedModelCallDisposition::KnownFailed,
                )),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","disposition":"known_failed"}}}"#,
    )?;
    assert_server_message_round_trip(
        request(24)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Failed {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: Some(uuid(7)),
                terminal_model_call: Some(FailedTerminalModelCall::new(
                    uuid(8),
                    FailedModelCallDisposition::Cancelled,
                )),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","disposition":"cancelled"}}}"#,
    )?;
    assert_server_message_round_trip(
        request(25)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Cancelled {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: uuid(7),
                terminal_model_call_id: None,
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"cancelled","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":null}}"#,
    )?;
    assert_server_message_round_trip(
        request(26)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Cancelled {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: uuid(7),
                terminal_model_call_id: Some(uuid(8)),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"cancelled","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
    )?;
    assert_server_message_round_trip(
        request(27)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::ReconciliationRequired {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: uuid(7),
                terminal_model_call_id: uuid(8),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"reconciliation_required","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
    )?;
    assert_server_message_round_trip(
        request(8)?,
        ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: uuid(1),
            entry_id: uuid(9),
            entry: TranscriptEntry::TurnCompleted { turn_id: uuid(3) },
        },
        r#"{"type":"transcript_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000009","entry":{"type":"turn_completed","turn_id":"00000000-0000-0000-0000-000000000003"}}"#,
    )?;
    assert_server_message_round_trip(
        request(28)?,
        ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: uuid(1),
            entry_id: uuid(9),
            entry: TranscriptEntry::TurnCancelled { turn_id: uuid(3) },
        },
        r#"{"type":"transcript_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000009","entry":{"type":"turn_cancelled","turn_id":"00000000-0000-0000-0000-000000000003"}}"#,
    )?;
    assert_server_message_round_trip(
        request(9)?,
        ServerMessage::TranscriptTextEntry {
            entry_index: CanonicalU64::new(1),
            source_session_id: uuid(1),
            entry_id: uuid(10),
            entry: TranscriptTextEntry::Assistant {
                turn_id: uuid(3),
                model_call_id: uuid(8),
            },
        },
        r#"{"type":"transcript_text_entry","entry_index":"1","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-00000000000a","entry":{"type":"assistant","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
    )?;
    assert_server_message_round_trip(
        request(10)?,
        ServerMessage::TranscriptContent {
            entry_index: CanonicalU64::new(1),
            fragment_index: CanonicalU64::new(0),
            final_fragment: true,
            content_fragment: ContentFragment::try_new("reply".to_owned())?,
        },
        r#"{"type":"transcript_content","entry_index":"1","fragment_index":"0","final_fragment":true,"content_fragment":"reply"}"#,
    )?;
    assert_server_message_round_trip(
        request(11)?,
        ServerMessage::TranscriptSnapshotEnd {
            session_id: uuid(1),
            cursor: CanonicalU64::new(5),
            turn_count: CanonicalU64::new(1),
            entry_count: CanonicalU64::new(2),
        },
        r#"{"type":"transcript_snapshot_end","session_id":"00000000-0000-0000-0000-000000000001","cursor":"5","turn_count":"1","entry_count":"2"}"#,
    )?;
    assert_server_message_round_trip(
        request(12)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(6),
            session_id: uuid(1),
            event: SessionEvent::ModelCallTransition {
                turn_id: uuid(3),
                model_call_id: uuid(8),
                state: ModelCallState::Terminal {
                    disposition: ModelCallDisposition::Refused,
                },
            },
        },
        r#"{"type":"session_event","cursor":"6","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"model_call_transition","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"terminal","disposition":"refused"}}}"#,
    )?;
    assert_server_message_round_trip(
        request(38)?,
        ServerMessage::ProviderTextDelta {
            session_id: uuid(1),
            turn_id: uuid(3),
            model_call_id: uuid(8),
            part_index: CanonicalU64::new(2),
            content: ContentFragment::try_new(String::from("already [redacted]"))?,
        },
        r#"{"type":"provider_text_delta","session_id":"00000000-0000-0000-0000-000000000001","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008","part_index":"2","content":"already [redacted]"}"#,
    )?;
    assert_server_message_round_trip(
        request(29)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(6),
            session_id: uuid(1),
            event: SessionEvent::ModelCallTransition {
                turn_id: uuid(3),
                model_call_id: uuid(8),
                state: ModelCallState::CancellationRequested {},
            },
        },
        r#"{"type":"session_event","cursor":"6","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"model_call_transition","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"cancellation_requested"}}}"#,
    )?;
    assert_server_message_round_trip(
        request(31)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(7),
            session_id: uuid(1),
            event: SessionEvent::ToolBatchTransition {
                turn_id: uuid(3),
                model_call_id: uuid(8),
                state: ToolBatchState::ResultsProjected {
                    frontier_id: uuid(6),
                },
            },
        },
        r#"{"type":"session_event","cursor":"7","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"tool_batch_transition","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"results_projected","frontier_id":"00000000-0000-0000-0000-000000000006"}}}"#,
    )?;
    assert_server_message_round_trip(
        request(18)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(2),
            session_id: uuid(1),
            event: SessionEvent::InputAccepted {
                accepted_input_id: uuid(2),
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                content: UserInputContent::text("accepted request".to_owned()),
            },
        },
        r#"{"type":"session_event","cursor":"2","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"input_accepted","accepted_input_id":"00000000-0000-0000-0000-000000000002","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","content":[{"type":"text","text":"accepted request"}]}}"#,
    )?;
    assert_server_message_round_trip(
        request(19)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(3),
            session_id: uuid(1),
            event: SessionEvent::TurnActivated {
                turn_id: uuid(3),
                current_attempt_id: uuid(7),
            },
        },
        r#"{"type":"session_event","cursor":"3","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"turn_activated","turn_id":"00000000-0000-0000-0000-000000000003","current_attempt_id":"00000000-0000-0000-0000-000000000007"}}"#,
    )?;
    assert_server_message_round_trip(
        request(30)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(4),
            session_id: uuid(1),
            event: SessionEvent::TurnCancelled {
                turn_id: uuid(3),
                cancellation_entry_id: uuid(9),
                terminal_frontier_id: uuid(6),
            },
        },
        r#"{"type":"session_event","cursor":"4","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"turn_cancelled","turn_id":"00000000-0000-0000-0000-000000000003","cancellation_entry_id":"00000000-0000-0000-0000-000000000009","terminal_frontier_id":"00000000-0000-0000-0000-000000000006"}}"#,
    )?;
    assert_server_message_round_trip(
        request(31)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(5),
            session_id: uuid(1),
            event: SessionEvent::TurnReconciliationRequired {
                turn_id: uuid(3),
                model_call_id: uuid(8),
                terminal_frontier_id: uuid(6),
            },
        },
        r#"{"type":"session_event","cursor":"5","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"turn_reconciliation_required","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008","terminal_frontier_id":"00000000-0000-0000-0000-000000000006"}}"#,
    )?;
    assert_server_message_round_trip(
        request(13)?,
        ServerMessage::Error {
            code: ErrorCode::NotFound,
            message: "not found".to_owned(),
            detail: ErrorDetail::none(),
        },
        r#"{"type":"error","code":"not_found","message":"not found"}"#,
    )?;
    Ok(())
}

#[test]
fn model_capability_catalog_wire_vocabulary_is_exact() -> Result<(), Box<dyn std::error::Error>> {
    assert_client_request_round_trip(
        request(41)?,
        ClientRequest::ListModelCapabilities {},
        r#"{"type":"list_model_capabilities"}"#,
    )?;
    assert_server_message_round_trip(
        request(41)?,
        ServerMessage::ModelCapabilityItem {
            selection_id: uuid(4),
            capabilities: ModelCapabilities {
                reasoning_levels: vec![ReasoningLevel::Low, ReasoningLevel::XHigh],
                fast_mode_supported: true,
                service_tiers: vec![ServiceTier::OpenAi(OpenAiServiceTier::Priority)],
            },
        },
        r#"{"type":"model_capability_item","selection_id":"00000000-0000-0000-0000-000000000004","capabilities":{"reasoning_levels":["low","xhigh"],"fast_mode_supported":true,"service_tiers":[{"provider":"open_ai","value":"priority"}]}}"#,
    )?;
    Ok(())
}

#[test]
fn runner_projection_round_trips_complete_current_loss() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::TranscriptSnapshotStart {
            workspace_root_kind: None,
            repository_watch: None,
            session_id: uuid(1),
            cursor: CanonicalU64::new(9),
            runner: Some(RunnerProjection::try_new(
                RunnerProjectionSelector::CapabilityClass {
                    name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))?,
                },
                Some(uuid(2)),
                RunnerPlacementRevision::try_new(3).expect("the fixture revision is positive"),
                RunnerSandboxProfile::WorkspaceRestricted,
                Some(RunnerCredentialProfileName::try_new(String::from(
                    "readonly",
                ))?),
                Some(RunnerRepositoryKey::try_new(String::from("signalbox"))?),
                Some(RunnerWorkingDirectory::try_new(String::from(
                    "workspace/project",
                ))?),
                None,
                RunnerProjectionState::RunnerLost,
            )?),
        },
        r#"{"type":"transcript_snapshot_start","workspace_root_kind":null,"repository_watch":null,"session_id":"00000000-0000-0000-0000-000000000001","cursor":"9","runner":{"selector":{"type":"capability_class","name":"linux.workspace"},"runner_id":"00000000-0000-0000-0000-000000000002","placement_revision":"3","sandbox_profile":"workspace-restricted","credential_profile":"readonly","repository":"signalbox","working_directory":"workspace/project","connection_health":null,"state":"runner_lost"}}"#,
    )
}

#[test]
fn runner_projection_round_trips_pinned_suspect_health() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::TranscriptSnapshotStart {
            workspace_root_kind: None,
            repository_watch: None,
            session_id: uuid(1),
            cursor: CanonicalU64::new(9),
            runner: Some(RunnerProjection::try_new(
                RunnerProjectionSelector::Runner { runner_id: uuid(2) },
                Some(uuid(2)),
                RunnerPlacementRevision::try_new(3).expect("the fixture revision is positive"),
                RunnerSandboxProfile::WorkspaceRestricted,
                None,
                None,
                None,
                Some(RunnerConnectionHealth::Suspect),
                RunnerProjectionState::Pinned,
            )?),
        },
        r#"{"type":"transcript_snapshot_start","workspace_root_kind":null,"repository_watch":null,"session_id":"00000000-0000-0000-0000-000000000001","cursor":"9","runner":{"selector":{"type":"runner","runner_id":"00000000-0000-0000-0000-000000000002"},"runner_id":"00000000-0000-0000-0000-000000000002","placement_revision":"3","sandbox_profile":"workspace-restricted","credential_profile":null,"repository":null,"working_directory":null,"connection_health":"suspect","state":"pinned"}}"#,
    )
}

#[test]
fn runner_projection_rejects_cross_wired_exact_runner() {
    let selected = uuid(1);
    let current = uuid(2);
    let projection = RunnerProjection::try_new(
        RunnerProjectionSelector::Runner {
            runner_id: selected,
        },
        Some(current),
        RunnerPlacementRevision::try_new(1).expect("the fixture revision is positive"),
        RunnerSandboxProfile::WorkspaceRestricted,
        None,
        None,
        None,
        None,
        RunnerProjectionState::RunnerLostBeforePin,
    );

    assert_eq!(
        projection,
        Err(crate::CanonicalValueError::RunnerProjection)
    );
}

#[test]
fn runner_projection_rejects_loss_without_exact_runner() {
    let projection = RunnerProjection::try_new(
        RunnerProjectionSelector::CapabilityClass {
            name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))
                .expect("the fixture capability is valid"),
        },
        None,
        RunnerPlacementRevision::try_new(1).expect("the fixture revision is positive"),
        RunnerSandboxProfile::WorkspaceRestricted,
        None,
        None,
        None,
        None,
        RunnerProjectionState::RunnerLost,
    );

    assert_eq!(
        projection,
        Err(crate::CanonicalValueError::RunnerProjection)
    );
}

#[test]
fn runner_projection_rejects_capability_selector_for_pre_pin_loss() {
    let projection = RunnerProjection::try_new(
        RunnerProjectionSelector::CapabilityClass {
            name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))
                .expect("the fixture capability is valid"),
        },
        Some(uuid(1)),
        RunnerPlacementRevision::try_new(1).expect("the fixture revision is positive"),
        RunnerSandboxProfile::WorkspaceRestricted,
        None,
        None,
        None,
        None,
        RunnerProjectionState::RunnerLostBeforePin,
    );

    assert_eq!(
        projection,
        Err(crate::CanonicalValueError::RunnerProjection)
    );
}
