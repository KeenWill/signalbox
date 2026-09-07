//! Goal protocol tests.

use super::support::*;
use crate::*;

#[test]
fn termination_metadata_may_be_absent_but_not_null() -> Result<(), Box<dyn std::error::Error>> {
    for message in [
        ServerMessage::InputSubmitted {
            termination: None,
            session_id: uuid(3),
            accepted_input_id: uuid(4),
            acceptance_position: CanonicalU64::new(1),
            turn_id: uuid(5),
            model_settings: settings_snapshot_fixture(),
        },
        ServerMessage::GoalTransitionApplied {
            termination: None,
            session_id: uuid(3),
            event_ordinal: CanonicalU64::new(1),
            generation: CanonicalU64::new(1),
        },
    ] {
        let mut value = serde_json::to_value(&message)?;
        assert_eq!(
            serde_json::from_value::<ServerMessage>(value.clone())?,
            message
        );
        value["termination"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<ServerMessage>(value.clone()).is_err(),
            "{value}"
        );
        value["termination"] = serde_json::to_value(TerminationReceipt {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            descendant_count: CanonicalU64::new(u64::MAX),
        })?;
        assert!(serde_json::from_value::<ServerMessage>(value).is_ok());
    }
    Ok(())
}

#[test]
fn parent_alone_receipt_rejects_descendant_dispositions() {
    let receipt = ServerMessage::GoalTransitionApplied {
        session_id: uuid(3),
        event_ordinal: CanonicalU64::new(1),
        generation: CanonicalU64::new(1),
        termination: Some(TerminationReceipt {
            descendant_scope: DescendantTerminationScope::ParentAlone,
            descendant_count: CanonicalU64::new(1),
        }),
    };
    assert_eq!(
        ServerFrame::try_new(request(1).expect("fixture request"), receipt)
            .expect_err("parent-alone never evaluates descendants"),
        FrameValidationError::DelegationShape
    );
}

#[test]
fn cascade_receipt_preserves_full_width_count_and_rejects_extra_fields()
-> Result<(), Box<dyn std::error::Error>> {
    let receipt = TerminationReceipt {
        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        descendant_count: CanonicalU64::new(u64::MAX),
    };
    let encoded = serde_json::to_value(receipt)?;
    assert_eq!(
        encoded,
        serde_json::json!({
            "descendant_scope": "parent_and_descendants",
            "descendant_count": "18446744073709551615",
        })
    );
    assert_eq!(
        serde_json::from_value::<TerminationReceipt>(encoded.clone())?,
        receipt
    );
    let mut extra = encoded;
    extra["unadmitted"] = serde_json::json!(true);
    assert!(serde_json::from_value::<TerminationReceipt>(extra).is_err());
    Ok(())
}

#[test]
fn goal_requests_and_history_round_trip_in_the_single_vocabulary()
-> Result<(), Box<dyn std::error::Error>> {
    assert_client_request_round_trip(
        request(1)?,
        ClientRequest::AttachGoal {
            command_id: command(2)?,
            session_id: uuid(3),
            statement: String::from("ship goal mode"),
        },
        r#"{"type":"attach_goal","command_id":"00000000-0000-0000-0000-000000000002","session_id":"00000000-0000-0000-0000-000000000003","statement":"ship goal mode"}"#,
    )?;
    assert_client_request_round_trip(
        request(4)?,
        ClientRequest::ResumeGoal {
            command_id: command(5)?,
            session_id: uuid(3),
            guidance: Some(String::from("use the user decision")),
        },
        r#"{"type":"resume_goal","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000003","guidance":"use the user decision"}"#,
    )?;
    assert_client_request_round_trip(
        request(6)?,
        ClientRequest::SupersedeGoal {
            command_id: command(7)?,
            session_id: uuid(3),
            statement: String::from("ship clarified goal mode"),
        },
        r#"{"type":"supersede_goal","command_id":"00000000-0000-0000-0000-000000000007","session_id":"00000000-0000-0000-0000-000000000003","statement":"ship clarified goal mode"}"#,
    )?;
    assert_client_request_round_trip(
        request(9)?,
        ClientRequest::StopSession {
            command_id: command(10)?,
            session_id: uuid(3),
            sticky: true,
            descendant_scope: DescendantTerminationScope::ParentAlone,
        },
        r#"{"type":"stop_session","command_id":"00000000-0000-0000-0000-00000000000a","session_id":"00000000-0000-0000-0000-000000000003","sticky":true,"descendant_scope":"parent_alone"}"#,
    )?;
    assert_client_request_round_trip(
        request(9)?,
        ClientRequest::ReleaseStart {
            command_id: command(14)?,
            session_id: uuid(3),
        },
        r#"{"type":"release_start","command_id":"00000000-0000-0000-0000-00000000000e","session_id":"00000000-0000-0000-0000-000000000003"}"#,
    )?;
    assert_client_request_round_trip(
        request(9)?,
        ClientRequest::CloseSessionFailed {
            command_id: command(11)?,
            session_id: uuid(3),
            cause: None,
        },
        r#"{"type":"close_session_failed","command_id":"00000000-0000-0000-0000-00000000000b","session_id":"00000000-0000-0000-0000-000000000003","cause":null}"#,
    )?;
    assert_client_request_round_trip(
        request(9)?,
        ClientRequest::AdoptSession {
            command_id: command(12)?,
            session_id: uuid(3),
            finish_condition: Some(crate::FinishCondition::Declared {
                statement: String::from("the branch is green"),
            }),
        },
        r#"{"type":"adopt_session","command_id":"00000000-0000-0000-0000-00000000000c","session_id":"00000000-0000-0000-0000-000000000003","finish_condition":{"kind":"declared","statement":"the branch is green"}}"#,
    )?;
    assert_client_request_round_trip(
        request(9)?,
        ClientRequest::AdoptSession {
            command_id: command(13)?,
            session_id: uuid(3),
            finish_condition: Some(crate::FinishCondition::ExternalGate),
        },
        r#"{"type":"adopt_session","command_id":"00000000-0000-0000-0000-00000000000d","session_id":"00000000-0000-0000-0000-000000000003","finish_condition":{"kind":"external_gate"}}"#,
    )?;
    assert_server_message_round_trip(
        request(9)?,
        ServerMessage::SessionLifecycleCommandApplied {
            session_id: uuid(3),
            effect: SessionLifecycleEffect::StartReleased {},
        },
        r#"{"type":"session_lifecycle_command_applied","session_id":"00000000-0000-0000-0000-000000000003","effect":{"type":"start_released"}}"#,
    )?;
    assert_server_message_round_trip(
        request(9)?,
        ServerMessage::SessionLifecycleCommandApplied {
            session_id: uuid(3),
            effect: SessionLifecycleEffect::ClosurePending {
                live_turn_id: uuid(4),
            },
        },
        r#"{"type":"session_lifecycle_command_applied","session_id":"00000000-0000-0000-0000-000000000003","effect":{"type":"closure_pending","live_turn_id":"00000000-0000-0000-0000-000000000004"}}"#,
    )?;
    assert_server_message_round_trip(
        request(8)?,
        ServerMessage::GoalHistoryStart {
            session_id: uuid(3),
            current_generation: CanonicalU64::new(2),
            current_statement: String::from("ship clarified goal mode"),
        },
        r#"{"type":"goal_history_start","session_id":"00000000-0000-0000-0000-000000000003","current_generation":"2","current_statement":"ship clarified goal mode"}"#,
    )?;
    assert_server_message_round_trip(
        request(8)?,
        ServerMessage::GoalHistoryState {
            current_state: GoalLifecycleState::Pursuing {},
        },
        r#"{"type":"goal_history_state","current_state":{"type":"pursuing"}}"#,
    )?;
    assert_server_message_round_trip(
        request(9)?,
        ServerMessage::GoalHistoryItem {
            event_ordinal: CanonicalU64::new(3),
            generation: CanonicalU64::new(2),
            event: GoalHistoryEvent::Blocked {
                reason: GoalBlockedReason::ExecutionFailure,
                need: String::from("repair execution"),
                provenance: GoalBlockedProvenance::ExecutionFailure { turn_id: uuid(10) },
            },
        },
        r#"{"type":"goal_history_item","event_ordinal":"3","generation":"2","event":{"type":"blocked","reason":"execution_failure","need":"repair execution","provenance":{"type":"execution_failure","turn_id":"00000000-0000-0000-0000-00000000000a"}}}"#,
    )?;
    assert_client_request_round_trip(
        request(11)?,
        ClientRequest::ReadGoal {
            session_id: uuid(3),
        },
        r#"{"type":"read_goal","session_id":"00000000-0000-0000-0000-000000000003"}"#,
    )?;
    assert_client_request_round_trip(
        request(12)?,
        ClientRequest::StopGoal {
            command_id: command(13)?,
            session_id: uuid(3),
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        },
        r#"{"type":"stop_goal","command_id":"00000000-0000-0000-0000-00000000000d","session_id":"00000000-0000-0000-0000-000000000003","descendant_scope":"parent_and_descendants"}"#,
    )?;
    assert_server_message_round_trip(
        request(14)?,
        ServerMessage::GoalTransitionApplied {
            termination: None,
            session_id: uuid(3),
            event_ordinal: CanonicalU64::new(4),
            generation: CanonicalU64::new(2),
        },
        r#"{"type":"goal_transition_applied","session_id":"00000000-0000-0000-0000-000000000003","event_ordinal":"4","generation":"2"}"#,
    )?;
    assert_server_message_round_trip(
        request(15)?,
        ServerMessage::GoalHistoryEnd {
            event_count: CanonicalU64::new(4),
        },
        r#"{"type":"goal_history_end","event_count":"4"}"#,
    )?;
    assert_server_message_round_trip(
        request(16)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("goal command rejected"),
            detail: ErrorDetail::rejected(RejectionDetail::GoalCommandRejected {
                session_id: uuid(3),
                reason: GoalCommandRejection::AcceptancePositionExhausted,
            }),
        },
        r#"{"type":"error","code":"rejected","message":"goal command rejected","detail":{"type":"goal_command_rejected","session_id":"00000000-0000-0000-0000-000000000003","reason":"acceptance_position_exhausted"}}"#,
    )
}

#[test]
fn split_goal_projection_fits_maximally_escaped_text_frames()
-> Result<(), Box<dyn std::error::Error>> {
    let text = "\u{1}".repeat(MAX_CONTENT_FRAGMENT_BYTES);
    let start = ServerFrame::try_new(
        request(1)?,
        ServerMessage::GoalHistoryStart {
            session_id: uuid(2),
            current_generation: CanonicalU64::new(1),
            current_statement: text.clone(),
        },
    )?;
    let state = ServerFrame::try_new(
        request(1)?,
        ServerMessage::GoalHistoryState {
            current_state: GoalLifecycleState::Blocked {
                reason: GoalBlockedReason::ExternalChangeRequired,
                need: text,
            },
        },
    )?;
    let start_encoded = encode_server_line(&start)?;
    let state_encoded = encode_server_line(&state)?;

    assert!(start_encoded.len() < crate::MAX_FRAME_BYTES);
    assert!(state_encoded.len() < crate::MAX_FRAME_BYTES);
    assert_eq!(decode_server_line(&start_encoded)?, start);
    assert_eq!(decode_server_line(&state_encoded)?, state);
    Ok(())
}

#[test]
fn goal_history_rejects_model_provenance_for_execution_failure() {
    let mismatched = ServerMessage::GoalHistoryItem {
        event_ordinal: CanonicalU64::new(2),
        generation: CanonicalU64::new(1),
        event: GoalHistoryEvent::Blocked {
            reason: GoalBlockedReason::ExecutionFailure,
            need: String::from("repair execution"),
            provenance: GoalBlockedProvenance::Model {
                turn_id: uuid(3),
                tool_request_id: uuid(4),
            },
        },
    };

    assert_eq!(
        ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            mismatched,
        )
        .expect_err("scheduler-only reason rejects model provenance"),
        FrameValidationError::GoalShape
    );
}
