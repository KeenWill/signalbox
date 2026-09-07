//! Reconciliation and delegation events protocol tests.

use super::support::*;
use crate::*;

#[test]
fn single_vocabulary_admits_reconciliation_shapes() -> Result<(), Box<dyn std::error::Error>> {
    let model_reconciliation = ServerMessage::TranscriptTurn {
        turn_id: uuid(3),
        acceptance_position: CanonicalU64::new(1),
        model_settings: None,
        state: TurnState::ReconciliationRequired {
            terminal_frontier_id: uuid(6),
            terminal_attempt_id: uuid(7),
            terminal_model_call_id: uuid(8),
        },
    };
    let frame =
        ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, model_reconciliation)?;
    let encoded = encode_server_line(&frame)?;
    assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":1,"));
    assert_eq!(decode_server_line(&encoded)?, frame);

    let tool_reconciliation = ServerMessage::TranscriptTurn {
        turn_id: uuid(3),
        acceptance_position: CanonicalU64::new(1),
        model_settings: None,
        state: TurnState::ToolReconciliationRequired {
            terminal_frontier_id: uuid(6),
            terminal_attempt_id: uuid(7),
            terminal_tool_attempt_id: uuid(9),
        },
    };
    let frame =
        ServerFrame::try_new_for_version(ProtocolVersion::One, request(2)?, tool_reconciliation)?;
    assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::ActiveAwaitingToolRecovery {
                ended_attempt_id: uuid(7),
                recovery_tool_attempt_id: uuid(9),
                automatic_reconciliation_attempts: CanonicalU64::new(2),
                operator_action_required: false,
            },
        },
        concat!(
            "{\"type\":\"transcript_turn\",\"turn_id\":\"00000000-0000-0000-0000-000000000003\",",
            "\"acceptance_position\":\"1\",\"model_settings\":null,\"state\":{",
            "\"type\":\"active_awaiting_tool_recovery\",",
            "\"ended_attempt_id\":\"00000000-0000-0000-0000-000000000007\",",
            "\"recovery_tool_attempt_id\":\"00000000-0000-0000-0000-000000000009\",",
            "\"automatic_reconciliation_attempts\":\"2\",",
            "\"operator_action_required\":false}}"
        ),
    )?;
    Ok(())
}

/// queued goal retirement has one exact closed wire
/// shape and round-trips its immutable turn identity.
#[test]
fn goal_turn_retired_event_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let message = ServerMessage::SessionEvent {
        cursor: CanonicalU64::new(1),
        session_id: uuid(1),
        event: SessionEvent::GoalTurnRetired { turn_id: uuid(2) },
    };
    let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(3)?, message)?;

    assert_eq!(
        String::from_utf8(encode_server_line(&frame)?)?,
        concat!(
            "{\"version\":1,\"request_id\":\"3\",\"message\":{\"type\":\"session_event\",\"cursor\":\"1\",",
            "\"session_id\":\"00000000-0000-0000-0000-000000000001\",",
            "\"event\":{\"type\":\"goal_turn_retired\",",
            "\"turn_id\":\"00000000-0000-0000-0000-000000000002\"}}}\n"
        )
    );
    assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
    Ok(())
}

#[test]
fn delegation_client_requests_round_trip_their_closed_shapes()
-> Result<(), Box<dyn std::error::Error>> {
    const SPAWN_FRAME_REQUEST: u64 = 34;
    const AWAIT_FRAME_REQUEST: u64 = 35;
    const MESSAGE_FRAME_REQUEST: u64 = 36;
    let ids = delegation_wire_identities();

    assert_client_request_round_trip(
        request(SPAWN_FRAME_REQUEST)?,
        ClientRequest::SpawnSession {
            session_id: ids.parent_session,
            turn_id: ids.parent_turn,
            tool_request_id: ids.spawning_request,
            task: String::from("inspect the failure"),
            relationship: DelegationPolicy::Bound {
                on_parent_stopped: crate::BoundChildAction::Stop,
                on_parent_cancelled: crate::BoundChildAction::Cancel,
            },
        },
        r#"{"type":"spawn_session","session_id":"00000000-0000-0000-0000-000000000001","turn_id":"00000000-0000-0000-0000-000000000002","tool_request_id":"00000000-0000-0000-0000-000000000003","task":"inspect the failure","relationship":{"type":"bound","on_parent_stopped":"stop","on_parent_cancelled":"cancel"}}"#,
    )?;
    assert_client_request_round_trip(
        request(AWAIT_FRAME_REQUEST)?,
        ClientRequest::AwaitSession {
            session_id: ids.parent_session,
            turn_id: ids.parent_turn,
            tool_request_id: ids.await_request,
            child_session_id: ids.child_session,
            mode: DelegationWaitMode::Foreground,
        },
        r#"{"type":"await_session","session_id":"00000000-0000-0000-0000-000000000001","turn_id":"00000000-0000-0000-0000-000000000002","tool_request_id":"00000000-0000-0000-0000-000000000004","child_session_id":"00000000-0000-0000-0000-000000000005","mode":"foreground"}"#,
    )?;
    assert_client_request_round_trip(
        request(MESSAGE_FRAME_REQUEST)?,
        ClientRequest::SendSessionMessage {
            session_id: ids.child_session,
            turn_id: ids.child_message_turn,
            tool_request_id: ids.message_request,
            peer_session_id: ids.parent_session,
            content: String::from("status update"),
        },
        r#"{"type":"send_session_message","session_id":"00000000-0000-0000-0000-000000000005","turn_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007","peer_session_id":"00000000-0000-0000-0000-000000000001","content":"status update"}"#,
    )?;
    Ok(())
}

#[test]
fn delegation_receipts_round_trip_result_and_delivery_correlation()
-> Result<(), Box<dyn std::error::Error>> {
    const SPAWN_RECEIPT_REQUEST: u64 = 37;
    const AWAIT_RECEIPT_REQUEST: u64 = 38;
    const RESULT_RECEIPT_REQUEST: u64 = 39;
    const MESSAGE_RECEIPT_REQUEST: u64 = 40;
    let ids = delegation_wire_identities();

    assert_server_message_round_trip(
        request(SPAWN_RECEIPT_REQUEST)?,
        ServerMessage::SessionSpawned {
            tool_request_id: ids.spawning_request,
            child_session_id: ids.child_session,
            relationship: DelegationPolicy::Background {},
        },
        r#"{"type":"session_spawned","tool_request_id":"00000000-0000-0000-0000-000000000003","child_session_id":"00000000-0000-0000-0000-000000000005","relationship":{"type":"background"}}"#,
    )?;
    assert_server_message_round_trip(
        request(AWAIT_RECEIPT_REQUEST)?,
        ServerMessage::SessionAwaitRegistered {
            tool_request_id: ids.await_request,
            child_session_id: ids.child_session,
            mode: DelegationWaitMode::Background,
        },
        r#"{"type":"session_await_registered","tool_request_id":"00000000-0000-0000-0000-000000000004","child_session_id":"00000000-0000-0000-0000-000000000005","mode":"background"}"#,
    )?;
    assert_server_message_round_trip(
        request(RESULT_RECEIPT_REQUEST)?,
        ServerMessage::ChildResult {
            await_request_id: ids.await_request,
            spawning_request_id: ids.spawning_request,
            child_session_id: ids.child_session,
            outcome: DelegationOutcome::Returned,
            content: Some(String::from("done")),
            reason: DelegationReason::ChildCompleted,
            provenance: DelegationProvenance::ChildTurn {
                child_session_id: ids.child_session,
                child_turn_id: ids.terminal_child_turn,
            },
        },
        r#"{"type":"child_result","await_request_id":"00000000-0000-0000-0000-000000000004","spawning_request_id":"00000000-0000-0000-0000-000000000003","child_session_id":"00000000-0000-0000-0000-000000000005","outcome":"returned","content":"done","reason":"child_completed","provenance":{"type":"child_turn","child_session_id":"00000000-0000-0000-0000-000000000005","child_turn_id":"00000000-0000-0000-0000-000000000008"}}"#,
    )?;
    assert_server_message_round_trip(
        request(MESSAGE_RECEIPT_REQUEST)?,
        ServerMessage::SessionMessageSent {
            tool_request_id: ids.message_request,
            message_id: ids.message,
            direction: DelegationMessageDirection::ChildToParent,
            ordinal: CanonicalU64::new(2),
            delivery_sequence: CanonicalU64::new(7),
        },
        r#"{"type":"session_message_sent","tool_request_id":"00000000-0000-0000-0000-000000000007","message_id":"00000000-0000-0000-0000-000000000009","direction":"child_to_parent","ordinal":"2","delivery_sequence":"7"}"#,
    )?;
    Ok(())
}

#[test]
fn delegation_request_rejections_round_trip_closed_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    const NOT_EXECUTABLE_FRAME_REQUEST: u64 = 41;
    const ORDINAL_EXHAUSTED_FRAME_REQUEST: u64 = 42;
    const PREPARED_FRAME_REQUEST: u64 = 43;
    const APPROVED_FRAME_REQUEST: u64 = 44;
    let ids = delegation_wire_identities();

    assert_server_message_round_trip(
        request(NOT_EXECUTABLE_FRAME_REQUEST)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("delegation request is not executable"),
            detail: ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: ids.spawning_request,
                state: DelegationToolRequestState::AttemptEnded,
            }),
        },
        r#"{"type":"error","code":"rejected","message":"delegation request is not executable","detail":{"type":"delegation_tool_request_not_executable","tool_request_id":"00000000-0000-0000-0000-000000000003","state":"attempt_ended"}}"#,
    )?;
    assert_server_message_round_trip(
        request(PREPARED_FRAME_REQUEST)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("delegation request is not executable"),
            detail: ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: ids.message_request,
                state: DelegationToolRequestState::Prepared,
            }),
        },
        r#"{"type":"error","code":"rejected","message":"delegation request is not executable","detail":{"type":"delegation_tool_request_not_executable","tool_request_id":"00000000-0000-0000-0000-000000000007","state":"prepared"}}"#,
    )?;
    assert_server_message_round_trip(
        request(APPROVED_FRAME_REQUEST)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("delegation request is not executable"),
            detail: ErrorDetail::rejected(RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id: ids.await_request,
                state: DelegationToolRequestState::Approved,
            }),
        },
        r#"{"type":"error","code":"rejected","message":"delegation request is not executable","detail":{"type":"delegation_tool_request_not_executable","tool_request_id":"00000000-0000-0000-0000-000000000004","state":"approved"}}"#,
    )?;
    assert_server_message_round_trip(
        request(ORDINAL_EXHAUSTED_FRAME_REQUEST)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("delegation event ordinal exhausted"),
            detail: ErrorDetail::rejected(RejectionDetail::DelegationEventOrdinalExhausted {
                spawning_request_id: ids.spawning_request,
                last: CanonicalU64::new(u64::MAX),
            }),
        },
        &format!(
            "{{\"type\":\"error\",\"code\":\"rejected\",\"message\":\"delegation event ordinal exhausted\",\"detail\":{{\"type\":\"delegation_event_ordinal_exhausted\",\"spawning_request_id\":\"00000000-0000-0000-0000-000000000003\",\"last\":\"{}\"}}}}",
            u64::MAX
        ),
    )?;
    Ok(())
}

#[test]
fn delivery_sequence_exhaustion_round_trips_closed_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    const FRAME_REQUEST: u64 = 51;
    let ids = delegation_wire_identities();

    assert_server_message_round_trip(
        request(FRAME_REQUEST)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("delegation delivery sequence exhausted"),
            detail: ErrorDetail::rejected(RejectionDetail::DelegationDeliverySequenceExhausted {
                recipient_session_id: ids.child_session,
                last: CanonicalU64::new(u64::MAX),
            }),
        },
        &format!(
            "{{\"type\":\"error\",\"code\":\"rejected\",\"message\":\"delegation delivery sequence exhausted\",\"detail\":{{\"type\":\"delegation_delivery_sequence_exhausted\",\"recipient_session_id\":\"{}\",\"last\":\"{}\"}}}}",
            ids.child_session,
            u64::MAX
        ),
    )?;
    Ok(())
}

#[test]
fn message_identity_collision_round_trips_closed_evidence() -> Result<(), Box<dyn std::error::Error>>
{
    const FRAME_REQUEST: u64 = 54;
    let ids = delegation_wire_identities();

    assert_server_message_round_trip(
        request(FRAME_REQUEST)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("delegation message identity collision"),
            detail: ErrorDetail::rejected(RejectionDetail::DelegationMessageIdentityCollision {
                message_id: ids.message,
            }),
        },
        &format!(
            "{{\"type\":\"error\",\"code\":\"rejected\",\"message\":\"delegation message identity collision\",\"detail\":{{\"type\":\"delegation_message_identity_collision\",\"message_id\":\"{}\"}}}}",
            ids.message
        ),
    )?;
    Ok(())
}

#[test]
fn delivery_sequence_exhaustion_rejects_a_nonterminal_counter()
-> Result<(), Box<dyn std::error::Error>> {
    const FRAME_REQUEST: u64 = 52;
    let ids = delegation_wire_identities();
    let frame = ServerFrame::try_new(
        request(FRAME_REQUEST)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("delegation delivery sequence exhausted"),
            detail: ErrorDetail::rejected(RejectionDetail::DelegationDeliverySequenceExhausted {
                recipient_session_id: ids.child_session,
                last: CanonicalU64::new(u64::MAX - 1),
            }),
        },
    );

    assert_eq!(frame, Err(FrameValidationError::ErrorDetailShape));
    Ok(())
}

#[test]
fn delegation_request_content_validation_is_left_to_application_input()
-> Result<(), Box<dyn std::error::Error>> {
    const EMPTY_TASK_FRAME_REQUEST: u64 = 43;
    const NUL_MESSAGE_FRAME_REQUEST: u64 = 44;
    const OVERSIZED_TASK_FRAME_REQUEST: u64 = 45;
    let ids = delegation_wire_identities();
    let empty_task = ClientFrame::try_new(
        request(EMPTY_TASK_FRAME_REQUEST)?,
        ClientRequest::SpawnSession {
            session_id: ids.parent_session,
            turn_id: ids.parent_turn,
            tool_request_id: ids.spawning_request,
            task: String::new(),
            relationship: DelegationPolicy::Background {},
        },
    )?;
    let nul_message = ClientFrame::try_new(
        request(NUL_MESSAGE_FRAME_REQUEST)?,
        ClientRequest::SendSessionMessage {
            session_id: ids.child_session,
            turn_id: ids.child_message_turn,
            tool_request_id: ids.message_request,
            peer_session_id: ids.parent_session,
            content: String::from("status\0update"),
        },
    )?;
    let oversized_task = ClientFrame::try_new(
        request(OVERSIZED_TASK_FRAME_REQUEST)?,
        ClientRequest::SpawnSession {
            session_id: ids.parent_session,
            turn_id: ids.parent_turn,
            tool_request_id: ids.spawning_request,
            task: "x".repeat(MAX_CONTENT_FRAGMENT_BYTES + 1),
            relationship: DelegationPolicy::Background {},
        },
    )?;

    assert_eq!(
        decode_client_line(&encode_client_line(&empty_task)?)?,
        empty_task
    );
    assert_eq!(
        decode_client_line(&encode_client_line(&nul_message)?)?,
        nul_message
    );
    assert_eq!(
        decode_client_line(&encode_client_line(&oversized_task)?)?,
        oversized_task
    );
    Ok(())
}

#[test]
fn parent_caused_child_results_keep_policy_action_separate_from_parent_reason()
-> Result<(), Box<dyn std::error::Error>> {
    const STOPPED_BY_CANCEL_FRAME_REQUEST: u64 = 46;
    const CANCELLED_BY_STOP_FRAME_REQUEST: u64 = 47;
    let ids = delegation_wire_identities();
    let stopped_by_parent_cancel = ServerFrame::try_new(
        request(STOPPED_BY_CANCEL_FRAME_REQUEST)?,
        ServerMessage::ChildResult {
            await_request_id: ids.await_request,
            spawning_request_id: ids.spawning_request,
            child_session_id: ids.child_session,
            outcome: DelegationOutcome::Stopped,
            content: None,
            reason: DelegationReason::ParentCancelled,
            provenance: DelegationProvenance::ParentTurnCommand {
                parent_session_id: ids.parent_session,
                parent_turn_id: ids.parent_turn,
                command_id: ids.parent_command,
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
        },
    )?;
    let cancelled_by_parent_stop = ServerFrame::try_new(
        request(CANCELLED_BY_STOP_FRAME_REQUEST)?,
        ServerMessage::ChildResult {
            await_request_id: ids.await_request,
            spawning_request_id: ids.spawning_request,
            child_session_id: ids.child_session,
            outcome: DelegationOutcome::Cancelled,
            content: None,
            reason: DelegationReason::ParentStopped,
            provenance: DelegationProvenance::ParentTurnCommand {
                parent_session_id: ids.parent_session,
                parent_turn_id: ids.parent_turn,
                command_id: ids.parent_command,
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
        },
    )?;

    assert_eq!(
        decode_server_line(&encode_server_line(&stopped_by_parent_cancel)?)?,
        stopped_by_parent_cancel
    );
    assert_eq!(
        decode_server_line(&encode_server_line(&cancelled_by_parent_stop)?)?,
        cancelled_by_parent_stop
    );
    Ok(())
}

#[test]
fn child_result_rejects_repeated_spawn_and_await_request_identity()
-> Result<(), Box<dyn std::error::Error>> {
    const REPEATED_REQUEST_FRAME_REQUEST: u64 = 48;
    let ids = delegation_wire_identities();
    let repeated_request = ServerFrame::try_new(
        request(REPEATED_REQUEST_FRAME_REQUEST)?,
        ServerMessage::ChildResult {
            await_request_id: ids.spawning_request,
            spawning_request_id: ids.spawning_request,
            child_session_id: ids.child_session,
            outcome: DelegationOutcome::Returned,
            content: Some(String::from("done")),
            reason: DelegationReason::ChildCompleted,
            provenance: DelegationProvenance::ChildTurn {
                child_session_id: ids.child_session,
                child_turn_id: ids.terminal_child_turn,
            },
        },
    );

    assert_eq!(repeated_request, Err(FrameValidationError::DelegationShape));
    Ok(())
}

#[test]
fn message_receipt_rejects_zero_delivery_sequence() -> Result<(), Box<dyn std::error::Error>> {
    const ZERO_DELIVERY_FRAME_REQUEST: u64 = 49;
    let ids = delegation_wire_identities();
    let zero_delivery = ServerFrame::try_new(
        request(ZERO_DELIVERY_FRAME_REQUEST)?,
        ServerMessage::SessionMessageSent {
            tool_request_id: ids.message_request,
            message_id: ids.message,
            direction: DelegationMessageDirection::ParentToChild,
            ordinal: CanonicalU64::new(2),
            delivery_sequence: CanonicalU64::new(0),
        },
    );

    assert_eq!(zero_delivery, Err(FrameValidationError::DelegationShape));
    Ok(())
}

#[test]
fn await_registration_rejects_foreground_mode() -> Result<(), Box<dyn std::error::Error>> {
    const FOREGROUND_REGISTRATION_FRAME_REQUEST: u64 = 50;
    let ids = delegation_wire_identities();
    let foreground_registration = ServerFrame::try_new(
        request(FOREGROUND_REGISTRATION_FRAME_REQUEST)?,
        ServerMessage::SessionAwaitRegistered {
            tool_request_id: ids.await_request,
            child_session_id: ids.child_session,
            mode: DelegationWaitMode::Foreground,
        },
    );

    assert_eq!(
        foreground_registration,
        Err(FrameValidationError::DelegationShape)
    );
    Ok(())
}

#[test]
fn message_receipt_rejects_the_reserved_spawn_ordinal() -> Result<(), Box<dyn std::error::Error>> {
    const FRAME_REQUEST: u64 = 53;
    let ids = delegation_wire_identities();
    let receipt = ServerFrame::try_new(
        request(FRAME_REQUEST)?,
        ServerMessage::SessionMessageSent {
            tool_request_id: ids.message_request,
            message_id: ids.message,
            direction: DelegationMessageDirection::ParentToChild,
            ordinal: CanonicalU64::new(1),
            delivery_sequence: CanonicalU64::new(1),
        },
    );

    assert_eq!(receipt, Err(FrameValidationError::DelegationShape));
    Ok(())
}

#[test]
fn delegation_session_events_round_trip_their_closed_shapes()
-> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(40)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::ChildSpawned {
                spawning_request_id: uuid(2),
                child_session_id: uuid(3),
                relationship: DelegationPolicy::Background {},
            },
        },
        r#"{"type":"session_event","cursor":"1","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"child_spawned","spawning_request_id":"00000000-0000-0000-0000-000000000002","child_session_id":"00000000-0000-0000-0000-000000000003","relationship":{"type":"background"}}}"#,
    )?;
    assert_server_message_round_trip(
        request(41)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(2),
            session_id: uuid(1),
            event: SessionEvent::ChildWaiting {
                await_request_id: uuid(4),
                spawning_request_id: uuid(2),
                child_session_id: uuid(3),
                mode: DelegationWaitMode::Background,
            },
        },
        r#"{"type":"session_event","cursor":"2","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"child_waiting","await_request_id":"00000000-0000-0000-0000-000000000004","spawning_request_id":"00000000-0000-0000-0000-000000000002","child_session_id":"00000000-0000-0000-0000-000000000003","mode":"background"}}"#,
    )?;
    assert_server_message_round_trip(
        request(42)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(3),
            session_id: uuid(3),
            event: SessionEvent::SessionMessage {
                spawning_request_id: uuid(2),
                message_id: uuid(5),
                sender_session_id: uuid(1),
                recipient_session_id: uuid(3),
                ordinal: CanonicalU64::new(2),
                delivery_sequence: CanonicalU64::new(7),
                content: String::from("status"),
            },
        },
        r#"{"type":"session_event","cursor":"3","session_id":"00000000-0000-0000-0000-000000000003","event":{"type":"session_message","spawning_request_id":"00000000-0000-0000-0000-000000000002","message_id":"00000000-0000-0000-0000-000000000005","sender_session_id":"00000000-0000-0000-0000-000000000001","recipient_session_id":"00000000-0000-0000-0000-000000000003","ordinal":"2","delivery_sequence":"7","content":"status"}}"#,
    )?;
    assert_server_message_round_trip(
        request(43)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(4),
            session_id: uuid(1),
            event: SessionEvent::ChildResult {
                spawning_request_id: uuid(2),
                child_session_id: uuid(3),
                outcome: DelegationOutcome::Returned,
                content: Some(String::from("done")),
                reason: DelegationReason::ChildCompleted,
                provenance: DelegationProvenance::ChildTurn {
                    child_session_id: uuid(3),
                    child_turn_id: uuid(6),
                },
            },
        },
        r#"{"type":"session_event","cursor":"4","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"child_result","spawning_request_id":"00000000-0000-0000-0000-000000000002","child_session_id":"00000000-0000-0000-0000-000000000003","outcome":"returned","content":"done","reason":"child_completed","provenance":{"type":"child_turn","child_session_id":"00000000-0000-0000-0000-000000000003","child_turn_id":"00000000-0000-0000-0000-000000000006"}}}"#,
    )?;
    assert_server_message_round_trip(
        request(44)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(5),
            session_id: uuid(1),
            event: SessionEvent::ChildLifecycleDisposition {
                spawning_request_id: uuid(2),
                child_session_id: uuid(3),
                outcome: DelegationOutcome::Stopped,
                reason: DelegationReason::ParentStopped,
                provenance: DelegationProvenance::ParentTurnCommand {
                    parent_session_id: uuid(1),
                    parent_turn_id: uuid(7),
                    command_id: uuid(8),
                    descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                },
            },
        },
        r#"{"type":"session_event","cursor":"5","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"child_lifecycle_disposition","spawning_request_id":"00000000-0000-0000-0000-000000000002","child_session_id":"00000000-0000-0000-0000-000000000003","outcome":"stopped","reason":"parent_stopped","provenance":{"type":"parent_turn_command","parent_session_id":"00000000-0000-0000-0000-000000000001","parent_turn_id":"00000000-0000-0000-0000-000000000007","command_id":"00000000-0000-0000-0000-000000000008","descendant_scope":"parent_and_descendants"}}}"#,
    )?;
    Ok(())
}

/// Round trips one child-addressed lifecycle disposition through the frame
/// validator. The header session is the terminalized child, and the
/// canonical provenance names the commanding parent's descendant-scoped
/// turn command.
#[track_caller]
fn assert_child_addressed_disposition_round_trips(
    outcome: DelegationOutcome,
    reason: DelegationReason,
) -> Result<(), Box<dyn std::error::Error>> {
    let frame = ServerFrame::try_new(
        request(1)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(5),
            session_id: uuid(3),
            event: SessionEvent::ChildLifecycleDisposition {
                spawning_request_id: uuid(2),
                child_session_id: uuid(3),
                outcome,
                reason,
                provenance: DelegationProvenance::ParentTurnCommand {
                    parent_session_id: uuid(1),
                    parent_turn_id: uuid(7),
                    command_id: uuid(8),
                    descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                },
            },
        },
    )?;
    assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
    Ok(())
}

#[test]
fn child_addressed_lifecycle_disposition_round_trips_for_a_child_follower()
-> Result<(), Box<dyn std::error::Error>> {
    // A descendant cascade addresses the terminalization to the child
    // itself so live child followers observe it. A bound relationship maps
    // the parent verb through its own policy, so all four outcome and
    // reason pairs reach the child follower.
    assert_child_addressed_disposition_round_trips(
        DelegationOutcome::Stopped,
        DelegationReason::ParentStopped,
    )?;
    assert_child_addressed_disposition_round_trips(
        DelegationOutcome::Cancelled,
        DelegationReason::ParentCancelled,
    )?;
    assert_child_addressed_disposition_round_trips(
        DelegationOutcome::Stopped,
        DelegationReason::ParentCancelled,
    )?;
    assert_child_addressed_disposition_round_trips(
        DelegationOutcome::Cancelled,
        DelegationReason::ParentStopped,
    )?;
    Ok(())
}

#[test]
fn child_addressed_lifecycle_disposition_rejects_non_terminal_and_self_authored_shapes()
-> Result<(), Box<dyn std::error::Error>> {
    // Continue-running and already-terminal remain parent-addressed only:
    // they report a child the cascade did not terminalize.
    let child_addressed_continue = ServerFrame::try_new(
        request(1)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(3),
            event: SessionEvent::ChildLifecycleDisposition {
                spawning_request_id: uuid(2),
                child_session_id: uuid(3),
                outcome: DelegationOutcome::ContinueRunning,
                reason: DelegationReason::ParentStopped,
                provenance: DelegationProvenance::ParentTurnCommand {
                    parent_session_id: uuid(1),
                    parent_turn_id: uuid(7),
                    command_id: uuid(8),
                    descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                },
            },
        },
    );
    // A child-addressed row must carry a foreign parent's authority; it can
    // never name itself as the commanding parent.
    let self_commanded = ServerFrame::try_new(
        request(2)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(2),
            session_id: uuid(3),
            event: SessionEvent::ChildLifecycleDisposition {
                spawning_request_id: uuid(2),
                child_session_id: uuid(3),
                outcome: DelegationOutcome::Stopped,
                reason: DelegationReason::ParentStopped,
                provenance: DelegationProvenance::ParentTurnCommand {
                    parent_session_id: uuid(3),
                    parent_turn_id: uuid(7),
                    command_id: uuid(8),
                    descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                },
            },
        },
    );
    // The parent-alone scope carries no descendant authority either way.
    let child_addressed_parent_alone = ServerFrame::try_new(
        request(3)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(3),
            session_id: uuid(3),
            event: SessionEvent::ChildLifecycleDisposition {
                spawning_request_id: uuid(2),
                child_session_id: uuid(3),
                outcome: DelegationOutcome::Stopped,
                reason: DelegationReason::ParentStopped,
                provenance: DelegationProvenance::ParentTurnCommand {
                    parent_session_id: uuid(1),
                    parent_turn_id: uuid(7),
                    command_id: uuid(8),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            },
        },
    );

    assert_eq!(
        child_addressed_continue,
        Err(FrameValidationError::DelegationShape)
    );
    assert_eq!(self_commanded, Err(FrameValidationError::DelegationShape));
    assert_eq!(
        child_addressed_parent_alone,
        Err(FrameValidationError::DelegationShape)
    );
    Ok(())
}

#[test]
fn delegation_session_events_reject_contradictory_shapes() -> Result<(), Box<dyn std::error::Error>>
{
    let missing_returned_content = ServerFrame::try_new(
        request(45)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::ChildResult {
                spawning_request_id: uuid(2),
                child_session_id: uuid(3),
                outcome: DelegationOutcome::Returned,
                content: None,
                reason: DelegationReason::ChildCompleted,
                provenance: DelegationProvenance::ChildTurn {
                    child_session_id: uuid(3),
                    child_turn_id: uuid(6),
                },
            },
        },
    );
    let parent_alone_disposition = ServerFrame::try_new(
        request(46)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(2),
            session_id: uuid(1),
            event: SessionEvent::ChildLifecycleDisposition {
                spawning_request_id: uuid(2),
                child_session_id: uuid(3),
                outcome: DelegationOutcome::ContinueRunning,
                reason: DelegationReason::ParentStopped,
                provenance: DelegationProvenance::ParentTurnCommand {
                    parent_session_id: uuid(1),
                    parent_turn_id: uuid(7),
                    command_id: uuid(8),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            },
        },
    );
    let zero_message_sequence = ServerFrame::try_new(
        request(47)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(3),
            session_id: uuid(1),
            event: SessionEvent::SessionMessage {
                spawning_request_id: uuid(2),
                message_id: uuid(5),
                sender_session_id: uuid(1),
                recipient_session_id: uuid(3),
                ordinal: CanonicalU64::new(1),
                delivery_sequence: CanonicalU64::new(0),
                content: String::from("status"),
            },
        },
    );

    assert_eq!(
        missing_returned_content,
        Err(FrameValidationError::DelegationShape)
    );
    assert_eq!(
        parent_alone_disposition,
        Err(FrameValidationError::DelegationShape)
    );
    assert_eq!(
        zero_message_sequence,
        Err(FrameValidationError::DelegationShape)
    );
    Ok(())
}

#[test]
fn inherits_imported_transcript_and_tool_event_shapes() -> Result<(), Box<dyn std::error::Error>> {
    let imported = ServerMessage::TranscriptTextEntry {
        entry_index: CanonicalU64::new(0),
        source_session_id: uuid(1),
        entry_id: uuid(2),
        entry: TranscriptTextEntry::Imported {
            imported_conversation_id: uuid(3),
            imported_entry_id: uuid(4),
            source_speaker: ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::User,
            },
        },
    };
    let imported_frame =
        ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, imported)?;
    assert_eq!(
        decode_server_line(&encode_server_line(&imported_frame)?)?,
        imported_frame
    );

    let tool_event = ServerMessage::SessionEvent {
        cursor: CanonicalU64::new(1),
        session_id: uuid(1),
        event: SessionEvent::TurnToolReconciliationRequired {
            turn_id: uuid(2),
            tool_attempt_id: uuid(3),
            terminal_frontier_id: uuid(4),
        },
    };
    let tool_frame =
        ServerFrame::try_new_for_version(ProtocolVersion::One, request(2)?, tool_event)?;
    assert_eq!(
        decode_server_line(&encode_server_line(&tool_frame)?)?,
        tool_frame
    );
    Ok(())
}

/// an explicit null is not a member of the closed delivery vocabulary.
#[test]
fn submit_delivery_rejects_explicit_null() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"1","request":{"type":"submit_input","command_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000002","content":"content","expected_defaults_version":"1","delivery":null}}"#,
    );
}

/// steering has one exact closed shape.
#[test]
fn steering_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    let steering_request = ClientRequest::SubmitInput {
        command_id: command(1)?,
        session_id: uuid(2),
        content: UserInputContent::text(String::from("steering")),
        expected_defaults_version: None,
        model_settings: ModelSettingsOverlay::inherit_all(),
        delivery: Some(InputDelivery::Steer {
            expected_active_turn_id: uuid(3),
        }),
    };
    let steering_frame =
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, steering_request)?;
    let encoded = encode_client_line(&steering_frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        concat!(
            "{\"version\":1,\"request_id\":\"1\",\"request\":{",
            "\"type\":\"submit_input\",",
            "\"command_id\":\"00000000-0000-0000-0000-000000000001\",",
            "\"session_id\":\"00000000-0000-0000-0000-000000000002\",",
            "\"content\":[{\"type\":\"text\",\"text\":\"steering\"}],",
            "\"expected_defaults_version\":null,",
            "\"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
            "\"fast_mode\":{\"kind\":\"inherit\"},",
            "\"service_tier\":{\"kind\":\"inherit\"}},",
            "\"delivery\":{\"type\":\"steer\",",
            "\"expected_active_turn_id\":",
            "\"00000000-0000-0000-0000-000000000003\"}}}\n"
        )
    );
    assert_eq!(decode_client_line(&encoded)?, steering_frame);
    Ok(())
}

/// queueing carries its exact active-turn and defaults guards.
#[test]
fn queueing_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    let queue_frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(2)?,
        ClientRequest::SubmitInput {
            command_id: command(4)?,
            session_id: uuid(2),
            content: UserInputContent::text(String::from("queued")),
            expected_defaults_version: Some(CanonicalU64::new(7)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: Some(InputDelivery::Queue {
                expected_active_turn_id: uuid(3),
            }),
        },
    )?;
    let encoded = encode_client_line(&queue_frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        concat!(
            "{\"version\":1,\"request_id\":\"2\",\"request\":{",
            "\"type\":\"submit_input\",",
            "\"command_id\":\"00000000-0000-0000-0000-000000000004\",",
            "\"session_id\":\"00000000-0000-0000-0000-000000000002\",",
            "\"content\":[{\"type\":\"text\",\"text\":\"queued\"}],",
            "\"expected_defaults_version\":\"7\",",
            "\"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
            "\"fast_mode\":{\"kind\":\"inherit\"},",
            "\"service_tier\":{\"kind\":\"inherit\"}},",
            "\"delivery\":{\"type\":\"queue\",",
            "\"expected_active_turn_id\":",
            "\"00000000-0000-0000-0000-000000000003\"}}}\n"
        )
    );
    assert_eq!(decode_client_line(&encoded)?, queue_frame);
    Ok(())
}

/// explicit start-when-idle has one closed shape.
#[test]
fn explicit_start_when_idle_has_a_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(3)?,
        ClientRequest::SubmitInput {
            command_id: command(5)?,
            session_id: uuid(2),
            content: UserInputContent::text(String::from("start")),
            expected_defaults_version: Some(CanonicalU64::new(7)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: Some(InputDelivery::StartWhenIdle {}),
        },
    )?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        concat!(
            "{\"version\":1,\"request_id\":\"3\",\"request\":{",
            "\"type\":\"submit_input\",",
            "\"command_id\":\"00000000-0000-0000-0000-000000000005\",",
            "\"session_id\":\"00000000-0000-0000-0000-000000000002\",",
            "\"content\":[{\"type\":\"text\",\"text\":\"start\"}],",
            "\"expected_defaults_version\":\"7\",",
            "\"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
            "\"fast_mode\":{\"kind\":\"inherit\"},",
            "\"service_tier\":{\"kind\":\"inherit\"}},",
            "\"delivery\":{\"type\":\"start_when_idle\"}}}\n"
        )
    );
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

/// configured start and queue treatments reject a missing
/// defaults guard before encoding.
#[test]
fn configured_delivery_rejects_missing_defaults() -> Result<(), Box<dyn std::error::Error>> {
    let start = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(4)?,
        ClientRequest::SubmitInput {
            command_id: command(6)?,
            session_id: uuid(2),
            content: UserInputContent::text(String::from("start without defaults")),
            expected_defaults_version: None,
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: Some(InputDelivery::StartWhenIdle {}),
        },
    );
    assert_eq!(start, Err(FrameValidationError::InputDeliveryShape));

    let queue = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(5)?,
        ClientRequest::SubmitInput {
            command_id: command(7)?,
            session_id: uuid(2),
            content: UserInputContent::text(String::from("queue without defaults")),
            expected_defaults_version: None,
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: Some(InputDelivery::Queue {
                expected_active_turn_id: uuid(3),
            }),
        },
    );
    assert_eq!(queue, Err(FrameValidationError::InputDeliveryShape));
    Ok(())
}

/// configuration-free steering rejects an independently supplied
/// defaults version before encoding.
#[test]
fn steering_rejects_independent_defaults_configuration() -> Result<(), Box<dyn std::error::Error>> {
    let invalid = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(3)?,
        ClientRequest::SubmitInput {
            command_id: command(5)?,
            session_id: uuid(2),
            content: UserInputContent::text(String::from("misconfigured steering")),
            expected_defaults_version: Some(CanonicalU64::new(7)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: Some(InputDelivery::Steer {
                expected_active_turn_id: uuid(3),
            }),
        },
    );
    assert_eq!(invalid, Err(FrameValidationError::InputDeliveryShape));

    let zero = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(4)?,
        ClientRequest::SubmitInput {
            command_id: command(6)?,
            session_id: uuid(2),
            content: UserInputContent::text(String::from("zero-version steering")),
            expected_defaults_version: Some(CanonicalU64::new(0)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: Some(InputDelivery::Steer {
                expected_active_turn_id: uuid(3),
            }),
        },
    );
    assert_eq!(zero, Err(FrameValidationError::InputDeliveryShape));
    Ok(())
}

/// steering against an already-stopping turn carries the exact
#[test]
fn stopping_steering_rejection_has_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(4)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the active turn is already stopping"),
            detail: ErrorDetail::rejected(RejectionDetail::SafePointUnavailableWhileStopping {
                session_id: uuid(2),
                active_turn_id: uuid(3),
                existing_command_id: uuid(5),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"the active turn is already stopping","detail":{"type":"safe_point_unavailable_while_stopping","session_id":"00000000-0000-0000-0000-000000000002","active_turn_id":"00000000-0000-0000-0000-000000000003","existing_command_id":"00000000-0000-0000-0000-000000000005"}}"#,
    )
}

/// its accepted input, position, and exact source turn.
#[test]
fn steering_receipt_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    let steering_response = ServerMessage::SteeringSubmitted {
        session_id: uuid(2),
        accepted_input_id: uuid(6),
        acceptance_position: CanonicalU64::new(8),
        source_turn_id: uuid(3),
    };
    let response_frame =
        ServerFrame::try_new_for_version(ProtocolVersion::One, request(4)?, steering_response)?;
    let encoded = encode_server_line(&response_frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        concat!(
            "{\"version\":1,\"request_id\":\"4\",\"message\":{",
            "\"type\":\"steering_submitted\",",
            "\"session_id\":\"00000000-0000-0000-0000-000000000002\",",
            "\"accepted_input_id\":",
            "\"00000000-0000-0000-0000-000000000006\",",
            "\"acceptance_position\":\"8\",\"source_turn_id\":",
            "\"00000000-0000-0000-0000-000000000003\"}}\n"
        )
    );
    assert_eq!(decode_server_line(&encoded)?, response_frame);
    Ok(())
}

#[test]
fn submit_content_bound_is_enforced_before_wire_encoding() -> Result<(), Box<dyn std::error::Error>>
{
    let content = "x".repeat(MAX_CONTENT_FRAGMENT_BYTES + 1);
    let result = ClientFrame::try_new(
        request(1)?,
        ClientRequest::SubmitInput {
            command_id: command(5)?,
            session_id: uuid(6),
            content: UserInputContent::text(content),
            expected_defaults_version: Some(CanonicalU64::new(1)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: None,
        },
    );
    assert_eq!(result, Err(FrameValidationError::UserContentShape));
    Ok(())
}
