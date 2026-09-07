//! Delegation turns and model calls protocol tests.

use super::support::*;
use crate::*;

#[test]
fn delegated_queued_turn_round_trips_exact_origin_provenance()
-> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(1),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::QueuedDelegated {
                spawning_request_id: uuid(2),
                parent_session_id: uuid(3),
                parent_turn_id: uuid(4),
                content: InputContent::new(String::from("delegated task")),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"queued_delegated","spawning_request_id":"00000000-0000-0000-0000-000000000002","parent_session_id":"00000000-0000-0000-0000-000000000003","parent_turn_id":"00000000-0000-0000-0000-000000000004","content":"delegated task"}}"#,
    )
}

#[test]
fn delegation_wake_queued_turn_round_trips_exact_delivery_range()
-> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(1),
            acceptance_position: CanonicalU64::new(2),
            model_settings: None,
            state: TurnState::QueuedDelegationWake {
                first_delivery_sequence: CanonicalU64::new(3),
                through_delivery_sequence: CanonicalU64::new(5),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"2","model_settings":null,"state":{"type":"queued_delegation_wake","first_delivery_sequence":"3","through_delivery_sequence":"5"}}"#,
    )
}

#[test]
fn delegation_wake_queued_turn_rejects_invalid_delivery_ranges() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"2","model_settings":null,"state":{"type":"queued_delegation_wake","first_delivery_sequence":"0","through_delivery_sequence":"5"}}}"#,
    );
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"2","model_settings":null,"state":{"type":"queued_delegation_wake","first_delivery_sequence":"5","through_delivery_sequence":"3"}}}"#,
    );
}

#[test]
fn nested_unit_shapes_reject_unknown_members() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"sessions_start","extra":true}}"#,
    );
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"queued","accepted_input_id":"00000000-0000-0000-0000-000000000002","content":[{"type":"text","text":"queued"}],"extra":true}}}"#,
    );
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"session_event","cursor":"1","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"session_created","extra":true}}}"#,
    );
}

#[test]
fn active_running_requires_current_model_call_member() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000002"}}}"#,
    );
}

#[test]
fn failed_terminal_shape_requires_nullable_attempt_member() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_model_call":null}}}"#,
    );
}

#[test]
fn failed_terminal_shape_requires_nullable_call_member() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":null}}}"#,
    );
}

#[test]
fn failed_terminal_call_requires_an_attempt() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":null,"terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000003","disposition":"known_failed"}}}}"#,
    );
}

#[test]
fn failed_terminal_call_accepts_only_failure_dispositions() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"completed"}}}}"#,
    );
}

#[test]
fn failed_terminal_call_rejects_unknown_members() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"known_failed","extra":true}}}}"#,
    );
}

#[test]
fn failed_terminal_call_cause_is_a_closed_wire_classification()
-> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(91)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(1),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Failed {
                terminal_frontier_id: uuid(2),
                terminal_attempt_id: Some(uuid(3)),
                terminal_model_call: Some(FailedTerminalModelCall::known_failed_with_cause(
                    uuid(4),
                    FailedModelCallCause::QuotaExhausted,
                )),
            },
        },
        r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"known_failed","cause":"quota_exhausted"}}}"#,
    )?;
    Ok(())
}

fn assert_attachment_failure_cause_round_trip(
    cause: FailedModelCallCause,
    spelling: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let encoded = encode_server_line(&ServerFrame {
        version: ProtocolVersion::One,
        request_id: request(92)?,
        message: ServerMessage::TranscriptTurn {
            turn_id: uuid(1),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Failed {
                terminal_frontier_id: uuid(2),
                terminal_attempt_id: Some(uuid(3)),
                terminal_model_call: Some(FailedTerminalModelCall::known_failed_with_cause(
                    uuid(4),
                    cause,
                )),
            },
        },
    })?;
    assert!(std::str::from_utf8(&encoded)?.contains(&format!("\"cause\":\"{spelling}\"")));
    let decoded = decode_server_line(&encoded)?;
    let ServerMessage::TranscriptTurn {
        state:
            TurnState::Failed {
                terminal_model_call: Some(call),
                ..
            },
        ..
    } = decoded.message
    else {
        panic!("attachment failure fixture keeps its terminal call");
    };
    assert_eq!(call.cause(), Some(cause));
    Ok(())
}

#[test]
fn attachment_too_large_round_trips_as_a_closed_wire_classification()
-> Result<(), Box<dyn std::error::Error>> {
    assert_attachment_failure_cause_round_trip(
        FailedModelCallCause::AttachmentTooLarge,
        "attachment_too_large",
    )
}

#[test]
fn attachment_missing_round_trips_as_a_closed_wire_classification()
-> Result<(), Box<dyn std::error::Error>> {
    assert_attachment_failure_cause_round_trip(
        FailedModelCallCause::AttachmentMissing,
        "attachment_missing",
    )
}

#[test]
fn attachment_corrupt_round_trips_as_a_closed_wire_classification()
-> Result<(), Box<dyn std::error::Error>> {
    assert_attachment_failure_cause_round_trip(
        FailedModelCallCause::AttachmentCorrupt,
        "attachment_corrupt",
    )
}

#[test]
fn failed_terminal_call_rejects_an_unknown_failure_cause() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"known_failed","cause":"future_provider_error"}}}}"#,
    );
}

#[test]
fn failed_terminal_call_rejects_explicit_null_cause() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"known_failed","cause":null}}}}"#,
    );
}

#[test]
fn failed_terminal_call_rejects_a_cause_on_cancelled_disposition() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"cancelled","cause":"quota_exhausted"}}}}"#,
    );
}

#[test]
fn cancelled_terminal_shape_requires_nullable_call_member() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"cancelled","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003"}}}"#,
    );
}

#[test]
fn turn_cancelled_event_rejects_unknown_members() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"session_event","cursor":"1","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"turn_cancelled","turn_id":"00000000-0000-0000-0000-000000000002","cancellation_entry_id":"00000000-0000-0000-0000-000000000003","terminal_frontier_id":"00000000-0000-0000-0000-000000000004","extra":true}}}"#,
    );
}

#[test]
fn cancellation_requested_state_rejects_unknown_members() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"session_event","cursor":"1","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"model_call_transition","turn_id":"00000000-0000-0000-0000-000000000002","model_call_id":"00000000-0000-0000-0000-000000000003","state":{"type":"cancellation_requested","extra":true}}}}"#,
    );
}

#[test]
fn nested_terminal_duplicate_members_are_rejected() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":null,"terminal_model_call":null}}}"#,
    );
}

#[test]
fn in_memory_failed_terminal_call_requires_an_attempt() -> Result<(), Box<dyn std::error::Error>> {
    let invalid = ServerFrame::try_new(
        request(1)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(1),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Failed {
                terminal_frontier_id: uuid(2),
                terminal_attempt_id: None,
                terminal_model_call: Some(FailedTerminalModelCall::new(
                    uuid(3),
                    FailedModelCallDisposition::KnownFailed,
                )),
            },
        },
    )
    .expect_err("an in-memory failed call without its attempt must be rejected");
    assert_eq!(invalid, FrameValidationError::TurnStateShape);
    Ok(())
}
