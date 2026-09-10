//! Submit and turn control protocol tests.

use super::support::*;
use crate::*;

#[test]
fn submit_request_round_trips_in_the_single_vocabulary() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ClientRequest::SubmitInput {
            command_id: command(4)?,
            session_id: uuid(6),
            content: UserInputContent::text(String::from("ordinary work")),
            expected_defaults_version: Some(CanonicalU64::new(1)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: None,
        },
    )?;
    let encoded = encode_client_line(&frame)?;

    assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":1,"));
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

#[test]
fn turn_control_vocabulary_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ClientRequest::StopTurn {
            command_id: command(4)?,
            session_id: uuid(6),
            expected_active_turn_id: uuid(7),
            content: UserInputContent::text(String::from("continue after the stop")),
            expected_defaults_version: CanonicalU64::new(1),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            model_settings: ModelSettingsOverlay::inherit_all(),
        },
    )?;
    let encoded = encode_client_line(&frame)?;

    assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":1,"));
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

/// reconciliation has one exact closed request shape.
#[test]
fn reconcile_turn_request_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    let request_id = request(1)?;
    let request_value = ClientRequest::ReconcileTurn {
        command_id: command(4)?,
        session_id: uuid(6),
        expected_active_turn_id: uuid(7),
        content: UserInputContent::text(String::from("continue after reconciliation")),
        expected_defaults_version: CanonicalU64::new(1),
        model_settings: ModelSettingsOverlay::inherit_all(),
    };

    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"reconcile_turn\",\
         \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
         \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
         \"expected_active_turn_id\":\"00000000-0000-0000-0000-000000000007\",\
         \"content\":[{\"type\":\"text\",\"text\":\"continue after reconciliation\"}],\
         \"expected_defaults_version\":\"1\",\
         \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
         \"fast_mode\":{\"kind\":\"inherit\"},\
         \"service_tier\":{\"kind\":\"inherit\"}}}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

/// the reconciliation refusal and the stale-target rejection carry
/// their exact closed wire shapes.
#[test]
fn reconciliation_rejection_details_have_exact_closed_shapes()
-> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the named turn is not awaiting reconciliation"),
            detail: ErrorDetail::rejected(RejectionDetail::TurnNotAwaitingReconciliation {
                session_id: uuid(6),
                turn_id: uuid(7),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"the named turn is not awaiting reconciliation","detail":{"type":"turn_not_awaiting_reconciliation","session_id":"00000000-0000-0000-0000-000000000006","turn_id":"00000000-0000-0000-0000-000000000007"}}"#,
    )?;
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the expected active turn is stale"),
            detail: ErrorDetail::rejected(RejectionDetail::ActiveTurnMismatch {
                session_id: uuid(6),
                expected_active_turn_id: uuid(7),
                active_turn_id: uuid(8),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"the expected active turn is stale","detail":{"type":"active_turn_mismatch","session_id":"00000000-0000-0000-0000-000000000006","expected_active_turn_id":"00000000-0000-0000-0000-000000000007","active_turn_id":"00000000-0000-0000-0000-000000000008"}}"#,
    )?;
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("a racing decision already released the slot"),
            detail: ErrorDetail::rejected(RejectionDetail::NoActiveTurn {
                session_id: uuid(6),
                expected_active_turn_id: uuid(7),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"a racing decision already released the slot","detail":{"type":"no_active_turn","session_id":"00000000-0000-0000-0000-000000000006","expected_active_turn_id":"00000000-0000-0000-0000-000000000007"}}"#,
    )
}

#[test]
fn import_source_requires_canonical_padded_base64() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"1","request":{"type":"import_conversation","format":"codex_rollout_jsonl_v2","source":"AA"}}"#,
    );
    assert_client_malformed(
        r#"{"version":1,"request_id":"1","request":{"type":"import_conversation","format":"codex_rollout_jsonl_v2","source":"AB=="}}"#,
    );
    assert_client_malformed(
        r#"{"version":1,"request_id":"1","request":{"type":"import_conversation","format":"codex_rollout_jsonl_v2","source":"AA==="}}"#,
    );
}

#[test]
fn submit_exchange_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let request_id = request(1)?;
    let request_frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request_id,
        ClientRequest::SubmitInput {
            command_id: command(2)?,
            session_id: uuid(3),
            content: UserInputContent::text(String::from("content")),
            expected_defaults_version: Some(CanonicalU64::new(1)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: None,
        },
    )?;
    let encoded_request = encode_client_line(&request_frame)?;
    assert_eq!(decode_client_line(&encoded_request)?, request_frame);
    let response_frame = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        request_id,
        ServerMessage::InputSubmitted {
            termination: None,
            session_id: uuid(3),
            accepted_input_id: uuid(4),
            acceptance_position: CanonicalU64::new(1),
            turn_id: uuid(5),
            model_settings: settings_snapshot_fixture(),
        },
    )?;
    let encoded_response = encode_server_line(&response_frame)?;
    assert_eq!(decode_server_line(&encoded_response)?, response_frame);
    Ok(())
}

/// the cursorless provider-text message round trips exactly.
#[test]
fn provider_text_message_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let request_id = request(1)?;
    let request_frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request_id,
        ClientRequest::ReadReviewTarget { target_id: uuid(2) },
    )?;
    let encoded_request = encode_client_line(&request_frame)?;
    let delta = ServerMessage::ProviderTextDelta {
        session_id: uuid(3),
        turn_id: uuid(4),
        model_call_id: uuid(5),
        part_index: CanonicalU64::new(6),
        content: ContentFragment::try_new(String::from("already [redacted]"))?,
    };

    assert_eq!(decode_client_line(&encoded_request)?, request_frame);
    let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request_id, delta)?;
    let encoded_delta = encode_server_line(&frame)?;
    assert_eq!(decode_server_line(&encoded_delta)?, frame);
    Ok(())
}
