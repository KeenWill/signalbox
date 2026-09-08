//! Catalog and tools protocol tests.

use super::support::*;
use crate::*;
use signalbox_domain::ToolDecisionRationale;

/// One commissioned-session request carries its complete composite —
/// fence, statement, and first input — in one closed shape, and its
/// receipt names the created session and the fence record.
#[test]
fn commission_session_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    assert_client_request_round_trip(
        request(1)?,
        ClientRequest::CommissionSession {
            command_id: command(2)?,
            template_name: String::from("review-response"),
            fence: CommissionedSessionFence::PullRequest {
                repository: String::from("sample-user/sample-repository"),
                pull_request: CanonicalU64::new(12),
                head_sha: String::from("1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d"),
                head_repository: String::from("sample-user/sample-repository"),
                head_branch: String::from("agent/sample-feature"),
                base_branch: String::from("main"),
            },
            statement: String::from("Address the findings on pull request 12."),
            content: InputContent::new(String::from("Respond to the review threads.")),
        },
        concat!(
            "{\"type\":\"commission_session\",",
            "\"command_id\":\"00000000-0000-0000-0000-000000000002\",",
            "\"template_name\":\"review-response\",",
            "\"fence\":{\"target\":\"pull_request\",",
            "\"repository\":\"sample-user/sample-repository\",",
            "\"pull_request\":\"12\",",
            "\"head_sha\":\"1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d\",",
            "\"head_repository\":\"sample-user/sample-repository\",",
            "\"head_branch\":\"agent/sample-feature\",",
            "\"base_branch\":\"main\"},",
            "\"statement\":\"Address the findings on pull request 12.\",",
            "\"content\":\"Respond to the review threads.\"}"
        ),
    )?;
    assert_client_request_round_trip(
        request(3)?,
        ClientRequest::CommissionSession {
            command_id: command(4)?,
            template_name: String::from("branch-watch"),
            fence: CommissionedSessionFence::Branch {
                repository: String::from("sample-user/sample-repository"),
                branch: String::from("main"),
            },
            statement: String::from("Investigate the failing workflow on main."),
            content: InputContent::new(String::from("The nightly workflow failed.")),
        },
        concat!(
            "{\"type\":\"commission_session\",",
            "\"command_id\":\"00000000-0000-0000-0000-000000000004\",",
            "\"template_name\":\"branch-watch\",",
            "\"fence\":{\"target\":\"branch\",",
            "\"repository\":\"sample-user/sample-repository\",",
            "\"branch\":\"main\"},",
            "\"statement\":\"Investigate the failing workflow on main.\",",
            "\"content\":\"The nightly workflow failed.\"}"
        ),
    )?;
    assert_server_message_round_trip(
        request(5)?,
        ServerMessage::SessionCommissioned {
            session_id: uuid(6),
            dispatch_id: uuid(7),
        },
        concat!(
            "{\"type\":\"session_commissioned\",",
            "\"session_id\":\"00000000-0000-0000-0000-000000000006\",",
            "\"dispatch_id\":\"00000000-0000-0000-0000-000000000007\"}"
        ),
    )?;

    let zero_pull_request = ClientRequest::CommissionSession {
        command_id: command(8)?,
        template_name: String::from("review-response"),
        fence: CommissionedSessionFence::PullRequest {
            repository: String::from("sample-user/sample-repository"),
            pull_request: CanonicalU64::new(0),
            head_sha: String::from("1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d"),
            head_repository: String::from("sample-user/sample-repository"),
            head_branch: String::from("agent/sample-feature"),
            base_branch: String::from("main"),
        },
        statement: String::from("Address the findings."),
        content: InputContent::new(String::from("Respond.")),
    };
    assert_eq!(
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(9)?, zero_pull_request),
        Err(FrameValidationError::DispatchFenceShape)
    );

    let empty_statement = ClientRequest::CommissionSession {
        command_id: command(10)?,
        template_name: String::from("review-response"),
        fence: CommissionedSessionFence::Branch {
            repository: String::from("sample-user/sample-repository"),
            branch: String::from("main"),
        },
        statement: String::new(),
        content: InputContent::new(String::from("Respond.")),
    };
    assert_eq!(
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(11)?, empty_statement),
        Err(FrameValidationError::GoalShape)
    );

    let uppercase_template = ClientRequest::CommissionSession {
        command_id: command(12)?,
        template_name: String::from("Review-Response"),
        fence: CommissionedSessionFence::Branch {
            repository: String::from("sample-user/sample-repository"),
            branch: String::from("main"),
        },
        statement: String::from("Address the findings."),
        content: InputContent::new(String::from("Respond.")),
    };
    assert_eq!(
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(13)?, uppercase_template),
        Err(FrameValidationError::TemplateShape)
    );
    Ok(())
}

/// request shape, and a requested semantic position must be nonzero.
#[test]
fn compaction_request_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    let compact = ClientRequest::CompactSession {
        command_id: command(1)?,
        session_id: uuid(2),
        through_position: Some(CanonicalU64::new(7)),
    };

    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request(3)?, compact)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        concat!(
            "{\"version\":1,\"request_id\":\"3\",\"request\":{",
            "\"type\":\"compact_session\",",
            "\"command_id\":\"00000000-0000-0000-0000-000000000001\",",
            "\"session_id\":\"00000000-0000-0000-0000-000000000002\",",
            "\"through_position\":\"7\"}}\n"
        )
    );
    assert_eq!(decode_client_line(&encoded)?, frame);

    let zero = ClientRequest::CompactSession {
        command_id: command(4)?,
        session_id: uuid(5),
        through_position: Some(CanonicalU64::new(0)),
    };
    assert_eq!(
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(6)?, zero,),
        Err(FrameValidationError::ContextCompactionShape)
    );
    Ok(())
}

#[test]
fn import_outcomes_have_distinct_closed_shapes() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::ConversationImportInserted {
            imported_conversation_id: uuid(2),
        },
        r#"{"type":"conversation_import_inserted","imported_conversation_id":"00000000-0000-0000-0000-000000000002"}"#,
    )?;
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::ConversationImportAlreadyImported {
            imported_conversation_id: uuid(3),
        },
        r#"{"type":"conversation_import_already_imported","imported_conversation_id":"00000000-0000-0000-0000-000000000003"}"#,
    )?;
    Ok(())
}

/// its exact closed shape across one encode/decode round trip.
#[test]
fn stop_turn_request_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    let request_id = request(1)?;
    let request_value = ClientRequest::StopTurn {
        command_id: command(4)?,
        session_id: uuid(6),
        expected_active_turn_id: uuid(7),
        content: UserInputContent::text(String::from("continue after the stop")),
        expected_defaults_version: CanonicalU64::new(1),
        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        model_settings: ModelSettingsOverlay::inherit_all(),
    };

    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"stop_turn\",\
         \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
         \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
         \"expected_active_turn_id\":\"00000000-0000-0000-0000-000000000007\",\
         \"content\":[{\"type\":\"text\",\"text\":\"continue after the stop\"}],\
         \"expected_defaults_version\":\"1\",\
         \"descendant_scope\":\"parent_and_descendants\",\
         \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
         \"fast_mode\":{\"kind\":\"inherit\"},\
         \"service_tier\":{\"kind\":\"inherit\"}}}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

/// tool decisions keep exact wire forms across one round trip.
#[test]
fn decide_tool_request_has_exact_closed_decision_shapes() -> Result<(), Box<dyn std::error::Error>>
{
    let approval = ClientRequest::DecideToolRequest {
        command_id: command(4)?,
        session_id: uuid(6),
        tool_request_id: uuid(7),
        decision: ToolDecision::Approve {},
    };
    let approval_frame =
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, approval)?;
    let encoded_approval = encode_client_line(&approval_frame)?;
    assert_eq!(
        String::from_utf8(encoded_approval.clone())?,
        "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"decide_tool_request\",\
         \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
         \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
         \"tool_request_id\":\"00000000-0000-0000-0000-000000000007\",\
         \"decision\":{\"type\":\"approve\"}}}\n"
    );
    assert_eq!(decode_client_line(&encoded_approval)?, approval_frame);

    let denial_frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(2)?,
        ClientRequest::DecideToolRequest {
            command_id: command(5)?,
            session_id: uuid(6),
            tool_request_id: uuid(7),
            decision: ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            },
        },
    )?;
    let encoded_denial = encode_client_line(&denial_frame)?;
    assert_eq!(
        String::from_utf8(encoded_denial.clone())?,
        "{\"version\":1,\"request_id\":\"2\",\"request\":{\"type\":\"decide_tool_request\",\
         \"command_id\":\"00000000-0000-0000-0000-000000000005\",\
         \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
         \"tool_request_id\":\"00000000-0000-0000-0000-000000000007\",\
         \"decision\":{\"type\":\"deny\",\"reason\":\"writes outside the workspace\"}}}\n"
    );
    assert_eq!(decode_client_line(&encoded_denial)?, denial_frame);

    assert_client_malformed(
        r#"{"version":1,"request_id":"3","request":{"type":"decide_tool_request","command_id":"00000000-0000-0000-0000-000000000004","session_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"approve","reason":"approve carries no reason"}}}"#,
    );
    assert_client_malformed(
        r#"{"version":1,"request_id":"4","request":{"type":"decide_tool_request","command_id":"00000000-0000-0000-0000-000000000004","session_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"deny"}}}"#,
    );
    Ok(())
}

#[test]
fn tool_approval_user_approve_event_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(8),
            session_id: uuid(6),
            event: SessionEvent::ToolApprovalDecided {
                turn_id: uuid(7),
                tool_request_id: uuid(8),
                decision: ToolApprovalEventDecision::Approve {},
                decider: ToolApprovalEventDecider::User {
                    command_id: uuid(9),
                },
                rationale: None,
            },
        },
        r#"{"type":"session_event","cursor":"8","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"approve"},"decider":{"type":"user","command_id":"00000000-0000-0000-0000-000000000009"},"rationale":null}}"#,
    )
}

#[test]
fn tool_approval_user_deny_event_round_trips_with_reason() -> Result<(), Box<dyn std::error::Error>>
{
    assert_server_message_round_trip(
        request(15)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(9),
            session_id: uuid(6),
            event: SessionEvent::ToolApprovalDecided {
                turn_id: uuid(7),
                tool_request_id: uuid(8),
                decision: ToolApprovalEventDecision::Deny {
                    reason: Some(String::from("user declined")),
                },
                decider: ToolApprovalEventDecider::User {
                    command_id: uuid(9),
                },
                rationale: None,
            },
        },
        r#"{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":"user declined"},"decider":{"type":"user","command_id":"00000000-0000-0000-0000-000000000009"},"rationale":null}}"#,
    )
}

#[test]
fn tool_approval_delegate_deny_event_round_trips_null_reason_for_empty_derivation()
-> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(4)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(9),
            session_id: uuid(6),
            event: SessionEvent::ToolApprovalDecided {
                turn_id: uuid(7),
                tool_request_id: uuid(8),
                decision: ToolApprovalEventDecision::Deny { reason: None },
                decider: ToolApprovalEventDecider::Delegate {
                    model_selection_id: uuid(10),
                    model_call_id: uuid(11),
                },
                rationale: Some(String::from("   ")),
            },
        },
        r#"{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":null},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000a","model_call_id":"00000000-0000-0000-0000-00000000000b"},"rationale":"   "}}"#,
    )
}

#[test]
fn transcript_tool_approval_round_trips_historical_delegate_provenance()
-> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(16)?,
        ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(2),
            source_session_id: uuid(6),
            entry_id: uuid(7),
            entry: TranscriptEntry::AssistantToolUse {
                turn_id: uuid(8),
                model_call_id: uuid(9),
                tool_request_id: uuid(10),
                tool_name: String::from("publish"),
                arguments: String::from("{}"),
                approval: Some(TranscriptToolApproval {
                    decision: ToolApprovalEventDecision::Deny { reason: None },
                    decider: ToolApprovalEventDecider::Delegate {
                        model_selection_id: uuid(11),
                        model_call_id: uuid(12),
                    },
                    rationale: Some(String::from("   ")),
                }),
            },
        },
        r#"{"type":"transcript_entry","entry_index":"2","source_session_id":"00000000-0000-0000-0000-000000000006","entry_id":"00000000-0000-0000-0000-000000000007","entry":{"type":"assistant_tool_use","turn_id":"00000000-0000-0000-0000-000000000008","model_call_id":"00000000-0000-0000-0000-000000000009","tool_request_id":"00000000-0000-0000-0000-00000000000a","tool_name":"publish","arguments":"{}","approval":{"decision":{"type":"deny","reason":null},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000b","model_call_id":"00000000-0000-0000-0000-00000000000c"},"rationale":"   "}}}"#,
    )
}

#[test]
fn transcript_tool_approval_rejects_explicit_null() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"16","message":{"type":"transcript_entry","entry_index":"2","source_session_id":"00000000-0000-0000-0000-000000000006","entry_id":"00000000-0000-0000-0000-000000000007","entry":{"type":"assistant_tool_use","turn_id":"00000000-0000-0000-0000-000000000008","model_call_id":"00000000-0000-0000-0000-000000000009","tool_request_id":"00000000-0000-0000-0000-00000000000a","tool_name":"publish","arguments":"{}","approval":null}}}"#,
    );
}

#[test]
fn tool_approval_user_decider_rejects_delegate_rationale() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"5","message":{"type":"session_event","cursor":"8","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"approve"},"decider":{"type":"user","command_id":"00000000-0000-0000-0000-000000000009"},"rationale":"forged judge rationale"}}}"#,
    );
}

/// the override request carries its exact closed wire shape.
#[test]
fn override_denied_tool_request_has_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    let override_request = ClientRequest::OverrideDeniedToolRequest {
        command_id: command(4)?,
        session_id: uuid(6),
        tool_request_id: uuid(7),
    };
    let frame =
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, override_request)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"override_denied_tool_request\",\
         \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
         \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
         \"tool_request_id\":\"00000000-0000-0000-0000-000000000007\"}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);

    assert_client_malformed(
        r#"{"version":1,"request_id":"2","request":{"type":"override_denied_tool_request","command_id":"00000000-0000-0000-0000-000000000004","session_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"approve"}}}"#,
    );
    Ok(())
}

/// the override receipt and every override rejection carry their
/// exact closed wire shapes.
#[test]
fn override_denial_responses_have_exact_closed_shapes() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::ToolDenialOverridden {
            tool_request_id: uuid(7),
        },
        r#"{"type":"tool_denial_overridden","tool_request_id":"00000000-0000-0000-0000-000000000007"}"#,
    )?;
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the request carries no delegate denial"),
            detail: ErrorDetail::rejected(RejectionDetail::ToolRequestNotDelegateDenied {
                tool_request_id: uuid(7),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"the request carries no delegate denial","detail":{"type":"tool_request_not_delegate_denied","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
    )?;
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the denial is still resolving"),
            detail: ErrorDetail::rejected(RejectionDetail::ToolRequestNotTerminallyDenied {
                tool_request_id: uuid(7),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"the denial is still resolving","detail":{"type":"tool_request_not_terminally_denied","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
    )?;
    assert_server_message_round_trip(
        request(4)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("an override is already recorded for the denial"),
            detail: ErrorDetail::rejected(RejectionDetail::ToolDenialAlreadyOverridden {
                tool_request_id: uuid(7),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"an override is already recorded for the denial","detail":{"type":"tool_denial_already_overridden","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
    )
}

#[test]
fn tool_approval_user_override_event_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(5)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(9),
            session_id: uuid(6),
            event: SessionEvent::ToolApprovalDecided {
                turn_id: uuid(7),
                tool_request_id: uuid(8),
                decision: ToolApprovalEventDecision::Approve {},
                decider: ToolApprovalEventDecider::UserOverride {
                    command_id: uuid(9),
                    overridden_tool_request_id: uuid(12),
                },
                rationale: None,
            },
        },
        r#"{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"approve"},"decider":{"type":"user_override","command_id":"00000000-0000-0000-0000-000000000009","overridden_tool_request_id":"00000000-0000-0000-0000-00000000000c"},"rationale":null}}"#,
    )
}

/// A user-override decider is approve-only and carries no rationale: a
/// denial or a rationale under that decider is a malformed frame.
#[test]
fn tool_approval_user_override_decider_is_approve_only() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"6","message":{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":null},"decider":{"type":"user_override","command_id":"00000000-0000-0000-0000-000000000009","overridden_tool_request_id":"00000000-0000-0000-0000-00000000000c"},"rationale":null}}}"#,
    );
    assert_server_malformed(
        r#"{"version":1,"request_id":"7","message":{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"approve"},"decider":{"type":"user_override","command_id":"00000000-0000-0000-0000-000000000009","overridden_tool_request_id":"00000000-0000-0000-0000-00000000000c"},"rationale":"forged rationale"}}}"#,
    );
}

#[test]
fn tool_approval_delegate_decider_requires_rationale() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"6","message":{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":null},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000a","model_call_id":"00000000-0000-0000-0000-00000000000b"},"rationale":null}}}"#,
    );
}

#[test]
fn tool_approval_delegate_deny_event_round_trips_with_derived_reason()
-> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(4)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(9),
            session_id: uuid(6),
            event: SessionEvent::ToolApprovalDecided {
                turn_id: uuid(7),
                tool_request_id: uuid(8),
                decision: ToolApprovalEventDecision::Deny {
                    reason: Some(String::from("request exceeds the stated scope")),
                },
                decider: ToolApprovalEventDecider::Delegate {
                    model_selection_id: uuid(10),
                    model_call_id: uuid(11),
                },
                rationale: Some(String::from("request exceeds the stated scope")),
            },
        },
        r#"{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":"request exceeds the stated scope"},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000a","model_call_id":"00000000-0000-0000-0000-00000000000b"},"rationale":"request exceeds the stated scope"}}"#,
    )
}

#[test]
fn tool_approval_delegate_denial_rejects_underived_reason() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"7","message":{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":"forged user reason"},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000a","model_call_id":"00000000-0000-0000-0000-00000000000b"},"rationale":"bounded rationale"}}}"#,
    );
}

#[test]
fn tool_approval_delegate_rationale_rejects_oversize() {
    const RATIONALE_FILLER: &str = "x";
    let oversized_rationale = RATIONALE_FILLER.repeat(ToolDecisionRationale::MAX_UTF8_BYTES + 1);
    let oversized_frame = [
        r#"{"version":1,"request_id":"10","message":{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"approve"},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000a","model_call_id":"00000000-0000-0000-0000-00000000000b"},"rationale":""#,
        oversized_rationale.as_str(),
        r#""}}}"#,
    ]
    .concat();

    assert_server_malformed(&oversized_frame);
}

/// every stop rejection carries its exact closed wire shape.
#[test]
fn stop_rejection_details_have_exact_closed_shapes() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("no turn held the session slot"),
            detail: ErrorDetail::rejected(RejectionDetail::NoActiveTurn {
                session_id: uuid(6),
                expected_active_turn_id: uuid(7),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"no turn held the session slot","detail":{"type":"no_active_turn","session_id":"00000000-0000-0000-0000-000000000006","expected_active_turn_id":"00000000-0000-0000-0000-000000000007"}}"#,
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
            message: String::from("a stop was already applied"),
            detail: ErrorDetail::rejected(RejectionDetail::InterruptAlreadyApplied {
                session_id: uuid(6),
                active_turn_id: uuid(7),
                existing_command_id: uuid(9),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"a stop was already applied","detail":{"type":"interrupt_already_applied","session_id":"00000000-0000-0000-0000-000000000006","active_turn_id":"00000000-0000-0000-0000-000000000007","existing_command_id":"00000000-0000-0000-0000-000000000009"}}"#,
    )?;
    assert_server_message_round_trip(
        request(4)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the active turn awaits a tool decision"),
            detail: ErrorDetail::rejected(
                RejectionDetail::InterruptUnavailableWhileAwaitingApproval {
                    session_id: uuid(6),
                    active_turn_id: uuid(7),
                },
            ),
        },
        r#"{"type":"error","code":"rejected","message":"the active turn awaits a tool decision","detail":{"type":"interrupt_unavailable_while_awaiting_approval","session_id":"00000000-0000-0000-0000-000000000006","active_turn_id":"00000000-0000-0000-0000-000000000007"}}"#,
    )
}

/// the decision receipt and every decision rejection carry their
#[test]
fn tool_decision_responses_have_exact_closed_shapes() -> Result<(), Box<dyn std::error::Error>> {
    let approval_receipt = ServerMessage::ToolRequestDecided {
        tool_request_id: uuid(7),
        decision: ToolDecision::Approve {},
    };
    assert_server_message_round_trip(
        request(1)?,
        approval_receipt,
        r#"{"type":"tool_request_decided","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"approve"}}"#,
    )?;
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::ToolRequestDecided {
            tool_request_id: uuid(7),
            decision: ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            },
        },
        r#"{"type":"tool_request_decided","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"deny","reason":"writes outside the workspace"}}"#,
    )?;
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("no logical request had this identity"),
            detail: ErrorDetail::rejected(RejectionDetail::ToolRequestNotFound {
                tool_request_id: uuid(7),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"no logical request had this identity","detail":{"type":"tool_request_not_found","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
    )?;
    assert_server_message_round_trip(
        request(4)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the request already has a resolution"),
            detail: ErrorDetail::rejected(RejectionDetail::ToolRequestAlreadyResolved {
                tool_request_id: uuid(7),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"the request already has a resolution","detail":{"type":"tool_request_already_resolved","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
    )?;
    assert_server_message_round_trip(
        request(5)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("an earlier request awaits its decision"),
            detail: ErrorDetail::rejected(RejectionDetail::ToolRequestNotEarliestUndecided {
                tool_request_id: uuid(7),
                earliest_tool_request_id: uuid(8),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"an earlier request awaits its decision","detail":{"type":"tool_request_not_earliest_undecided","tool_request_id":"00000000-0000-0000-0000-000000000007","earliest_tool_request_id":"00000000-0000-0000-0000-000000000008"}}"#,
    )?;
    assert_server_message_round_trip(
        request(6)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the request is not owned by the session"),
            detail: ErrorDetail::rejected(RejectionDetail::ToolRequestNotInSession {
                session_id: uuid(6),
                tool_request_id: uuid(7),
            }),
        },
        r#"{"type":"error","code":"rejected","message":"the request is not owned by the session","detail":{"type":"tool_request_not_in_session","session_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
    )
}

#[test]
fn tool_closure_distinguishes_approved_and_undecided_requests()
-> Result<(), Box<dyn std::error::Error>> {
    for approved_before_close in [true, false] {
        let message = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: uuid(1),
            entry_id: uuid(2),
            entry: TranscriptEntry::ToolClosed {
                tool_request_id: uuid(3),
                content: String::from("closed before execution"),
                approved_before_close,
            },
        };
        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message)?;
        let encoded = encode_server_line(&frame)?;
        assert_eq!(decode_server_line(&encoded)?, frame);
        let mut missing_evidence: serde_json::Value = serde_json::from_slice(&encoded)?;
        missing_evidence["message"]["entry"]
            .as_object_mut()
            .expect("a transcript entry is an object")
            .remove("approved_before_close");
        let mut missing_evidence = serde_json::to_vec(&missing_evidence)?;
        missing_evidence.push(b'\n');
        assert!(decode_server_line(&missing_evidence).is_err());
    }
    Ok(())
}
