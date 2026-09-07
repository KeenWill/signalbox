//! Support protocol tests and fixtures.

use uuid::Uuid;

use crate::*;

pub(super) fn command(value: u128) -> Result<CommandId, Box<dyn std::error::Error>> {
    Ok(CommandId::try_from_uuid(Uuid::from_u128(value))?)
}

pub(super) fn request(value: u64) -> Result<RequestId, Box<dyn std::error::Error>> {
    Ok(RequestId::try_new(value)?)
}

pub(super) fn uuid(value: u128) -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::from_u128(value))
}

/// Arbitrary distinct identities whose field names preserve delegation wire roles.
#[derive(Clone, Copy)]
pub(super) struct DelegationWireIdentities {
    pub(super) parent_session: CanonicalUuid,
    pub(super) parent_turn: CanonicalUuid,
    pub(super) spawning_request: CanonicalUuid,
    pub(super) await_request: CanonicalUuid,
    pub(super) child_session: CanonicalUuid,
    pub(super) child_message_turn: CanonicalUuid,
    pub(super) message_request: CanonicalUuid,
    pub(super) terminal_child_turn: CanonicalUuid,
    pub(super) message: CanonicalUuid,
    pub(super) parent_command: CanonicalUuid,
}

pub(super) fn delegation_wire_identities() -> DelegationWireIdentities {
    DelegationWireIdentities {
        parent_session: uuid(1),
        parent_turn: uuid(2),
        spawning_request: uuid(3),
        await_request: uuid(4),
        child_session: uuid(5),
        child_message_turn: uuid(6),
        message_request: uuid(7),
        terminal_child_turn: uuid(8),
        message: uuid(9),
        parent_command: uuid(10),
    }
}

pub(super) fn settings_snapshot_fixture() -> ModelSettingsSnapshot {
    let per_call = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::High),
        fast_mode: FastModeOverlay::Inherit,
        service_tier: SettingOverlay::ProviderDefault,
    };
    let inherited = ModelSettingsOverlay::inherit_all();
    ModelSettingsSnapshot {
        precedence: ModelSettingsPrecedence {
            per_call,
            session: inherited,
            profile: inherited,
            global_default: inherited,
        },
        effective: EffectiveModelSettings {
            reasoning_level: Some(ReasoningLevel::High),
            fast_mode: FastMode::Disabled,
            service_tier: None,
        },
        reasoning_source: Some(ModelSettingSource::PerCall),
        fast_mode_source: None,
        service_tier_source: Some(ModelSettingSource::PerCall),
        validated_for_selection_id: Some(uuid(4)),
    }
}

pub(super) fn provider_default_settings_snapshot_fixture() -> ModelSettingsSnapshot {
    let inherited = ModelSettingsOverlay::inherit_all();
    ModelSettingsSnapshot {
        precedence: ModelSettingsPrecedence {
            per_call: inherited,
            session: inherited,
            profile: inherited,
            global_default: inherited,
        },
        effective: EffectiveModelSettings {
            reasoning_level: None,
            fast_mode: FastMode::Disabled,
            service_tier: None,
        },
        reasoning_source: None,
        fast_mode_source: None,
        service_tier_source: None,
        validated_for_selection_id: None,
    }
}

pub(super) fn session_settings_snapshot_fixture() -> ModelSettingsSnapshot {
    let inherited = ModelSettingsOverlay::inherit_all();
    let session = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::High),
        fast_mode: FastModeOverlay::Inherit,
        service_tier: SettingOverlay::ProviderDefault,
    };
    ModelSettingsSnapshot {
        precedence: ModelSettingsPrecedence {
            per_call: inherited,
            session,
            profile: inherited,
            global_default: inherited,
        },
        effective: EffectiveModelSettings {
            reasoning_level: Some(ReasoningLevel::High),
            fast_mode: FastMode::Disabled,
            service_tier: None,
        },
        reasoning_source: Some(ModelSettingSource::Session),
        fast_mode_source: None,
        service_tier_source: Some(ModelSettingSource::Session),
        validated_for_selection_id: Some(uuid(4)),
    }
}

pub(super) const SETTINGS_SNAPSHOT_JSON: &str = concat!(
    "{\"precedence\":{",
    "\"per_call\":{\"reasoning_level\":{\"kind\":\"value\",\"value\":\"high\"},",
    "\"fast_mode\":{\"kind\":\"inherit\"},",
    "\"service_tier\":{\"kind\":\"provider_default\"}},",
    "\"session\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
    "\"fast_mode\":{\"kind\":\"inherit\"},",
    "\"service_tier\":{\"kind\":\"inherit\"}},",
    "\"profile\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
    "\"fast_mode\":{\"kind\":\"inherit\"},",
    "\"service_tier\":{\"kind\":\"inherit\"}},",
    "\"global_default\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
    "\"fast_mode\":{\"kind\":\"inherit\"},",
    "\"service_tier\":{\"kind\":\"inherit\"}}},",
    "\"effective\":{\"reasoning_level\":\"high\",\"fast_mode\":\"disabled\",",
    "\"service_tier\":null},\"reasoning_source\":\"per_call\",",
    "\"fast_mode_source\":null,\"service_tier_source\":\"per_call\",",
    "\"validated_for_selection_id\":\"00000000-0000-0000-0000-000000000004\"}"
);

pub(super) const PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON: &str = concat!(
    "{\"precedence\":{",
    "\"per_call\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
    "\"fast_mode\":{\"kind\":\"inherit\"},",
    "\"service_tier\":{\"kind\":\"inherit\"}},",
    "\"session\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
    "\"fast_mode\":{\"kind\":\"inherit\"},",
    "\"service_tier\":{\"kind\":\"inherit\"}},",
    "\"profile\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
    "\"fast_mode\":{\"kind\":\"inherit\"},",
    "\"service_tier\":{\"kind\":\"inherit\"}},",
    "\"global_default\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
    "\"fast_mode\":{\"kind\":\"inherit\"},",
    "\"service_tier\":{\"kind\":\"inherit\"}}},",
    "\"effective\":{\"reasoning_level\":null,\"fast_mode\":\"disabled\",",
    "\"service_tier\":null},\"reasoning_source\":null,",
    "\"fast_mode_source\":null,\"service_tier_source\":null,",
    "\"validated_for_selection_id\":null}"
);

pub(super) fn orchestration_snapshot_fixture(
    state: ReviewOrchestrationState,
    status: ReviewOrchestrationConcernStatus,
    pass_id: Option<CanonicalUuid>,
    counts: ReviewOrchestrationCounts,
) -> Result<ReviewOrchestrationSnapshot, Box<dyn std::error::Error>> {
    let digest = CanonicalDigest::try_new("ab".repeat(32))?;
    Ok(ReviewOrchestrationSnapshot {
        attempt_id: uuid(3),
        target_id: uuid(4),
        state,
        concern_set_version: String::from("initial-five"),
        stage_template_digests: ReviewOrchestrationStageTemplateDigests {
            import: digest.clone(),
            judgment: digest.clone(),
            repair: digest.clone(),
            publication: digest.clone(),
        },
        concerns: vec![ReviewOrchestrationConcernSnapshot {
            key: String::from("correctness"),
            template_digest: digest,
            status,
            pass_id,
        }],
        counts,
    })
}

pub(super) fn metadata(archived: bool) -> Result<SessionMetadata, Box<dyn std::error::Error>> {
    Ok(SessionMetadata::try_new(
        Some(String::from("Planning")),
        vec![String::from("work"), String::from("daily")],
        vec![
            (String::from("run"), String::from("17")),
            (String::from("trigger"), String::new()),
        ],
        archived,
    )?)
}

pub(super) fn numbered_metadata_strings(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("value-{index}")).collect()
}

pub(super) fn numbered_metadata_attributes(count: usize) -> Vec<(String, String)> {
    numbered_metadata_strings(count)
        .into_iter()
        .map(|key| (key, String::new()))
        .collect()
}

pub(super) fn line(json: &str) -> Vec<u8> {
    let mut bytes = json.as_bytes().to_vec();
    bytes.push(b'\n');
    bytes
}

pub(super) fn padded_oversized_client_frame(request_members: &str, content_len: usize) -> Vec<u8> {
    let mut bytes = format!(
        r#"{{"version":1,{request_members},"request":{{"type":"list_sessions","padding":""#
    )
    .into_bytes();
    let suffix = b"\"}}";
    assert!(content_len >= bytes.len() + suffix.len());
    bytes.resize(content_len - suffix.len(), b'x');
    bytes.extend_from_slice(suffix);
    bytes.push(b'\n');
    assert_eq!(bytes.len(), content_len + 1);
    bytes
}

#[track_caller]
pub(super) fn assert_client_malformed(json: &str) {
    let error = decode_client_line(&line(json)).expect_err("client frame must be malformed");
    assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
}

#[track_caller]
pub(super) fn assert_server_malformed(json: &str) {
    let error = decode_server_line(&line(json)).expect_err("server frame must be malformed");
    assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
}

#[track_caller]
pub(super) fn assert_placement_version_mismatch_rejected(expected: u64, current: u64) {
    let error = ServerFrame::try_new(
        request(1).expect("fixture request identity is admitted"),
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("placement version mismatch"),
            detail: ErrorDetail::rejected(
                RejectionDetail::SessionPlacementCurrentVersionMismatch {
                    session_id: uuid(2),
                    expected_placement_version: CanonicalU64::new(expected),
                    current_placement_version: CanonicalU64::new(current),
                },
            ),
        },
    )
    .expect_err("incoherent placement mismatch evidence is rejected");
    assert_eq!(error, FrameValidationError::ErrorDetailShape);
}

pub(super) fn placement_version_exhaustion_frame(
    current: u64,
) -> Result<ServerFrame, FrameValidationError> {
    ServerFrame::try_new(
        request(1).expect("fixture request identity is admitted"),
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("placement version exhausted"),
            detail: ErrorDetail::rejected(RejectionDetail::SessionPlacementVersionExhausted {
                session_id: uuid(2),
                current_placement_version: CanonicalU64::new(current),
            }),
        },
    )
}

#[track_caller]
pub(super) fn assert_unsupported_version(version: &str) {
    let json = format!(
        "{{\"version\":{version},\"request_id\":\"9\",\"request\":{{\"type\":\"future_request\",\"anything\":true}}}}"
    );
    let error = decode_client_line(&line(&json)).expect_err("version must be unsupported");
    assert_eq!(error.kind(), FrameDecodeErrorKind::UnsupportedVersion);
    assert_eq!(error.request_id().value(), 9);
    assert!(error.to_string().contains("supported version is 1"));
}

pub(super) fn unsupported_version_with_nested_object_payload(payload_depth: usize) -> String {
    let payload = format!(
        "{}0{}",
        r#"{"future":"#.repeat(payload_depth),
        "}".repeat(payload_depth)
    );
    format!("{{\"version\":15,\"request_id\":\"9\",\"request\":{payload}}}")
}

#[track_caller]
pub(super) fn assert_command_sentinel_rejected(command_id: &str) {
    let json = format!(
        "{{\"version\":1,\"request_id\":\"1\",\"request\":{{\"type\":\"create_session\",\"command_id\":\"{command_id}\",\"initial_model_selection\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000001\"}}}}}}"
    );
    assert_client_malformed(&json);
}

#[track_caller]
pub(super) fn assert_client_request_current_version(
    request_id: RequestId,
    request: ClientRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let frame = ClientFrame::try_new(request_id, request)?;
    let encoded = String::from_utf8(encode_client_line(&frame)?)?;
    assert!(encoded.starts_with(&format!("{{\"version\":{PROTOCOL_VERSION},")));
    Ok(())
}

#[track_caller]
pub(super) fn assert_client_request_round_trip(
    request_id: RequestId,
    request: ClientRequest,
    expected_request_json: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request_id_value = request_id.value();
    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request)?;
    let encoded = encode_client_line(&frame)?;
    let expected = format!(
        "{{\"version\":{PROTOCOL_VERSION},\"request_id\":\"{request_id_value}\",\"request\":{expected_request_json}}}\n"
    );
    assert_eq!(String::from_utf8(encoded.clone())?, expected);
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

#[track_caller]
pub(super) fn assert_server_message_round_trip(
    request_id: RequestId,
    message: ServerMessage,
    expected_message_json: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request_id_value = request_id.value();
    let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request_id, message)?;
    let encoded = encode_server_line(&frame)?;
    let expected = format!(
        "{{\"version\":{PROTOCOL_VERSION},\"request_id\":\"{request_id_value}\",\"message\":{}}}\n",
        expected_message_json
    );
    assert_eq!(String::from_utf8(encoded.clone())?, expected);
    assert_eq!(decode_server_line(&encoded)?, frame);
    Ok(())
}

/// Rejects one `delegation_terminated` wire shape named by its outcome and
/// reason spelling. Every other member carries the canonical parent-goal
/// cascade the admitted shapes also use.
#[track_caller]
pub(super) fn assert_delegation_terminal_state_rejected(outcome: &str, reason: &str) {
    serde_json::from_value::<TurnState>(serde_json::json!({
        "type": "delegation_terminated",
        "spawning_request_id": "00000000-0000-0000-0000-000000000004",
        "outcome": outcome,
        "reason": reason,
        "provenance": {
            "type": "parent_goal_command",
            "parent_session_id": "00000000-0000-0000-0000-000000000001",
            "goal_generation": "2",
            "command_id": "00000000-0000-0000-0000-000000000007",
            "descendant_scope": "parent_and_descendants"
        }
    }))
    .expect_err("an inadmissible terminal outcome and reason pair must not decode");
}

/// Round trips one admitted `delegation_terminated` turn state through
/// serde and through the frame validator every transcript read and initial
/// follow snapshot runs.
#[track_caller]
pub(super) fn assert_delegation_terminal_state_round_trips(
    outcome: DelegationOutcome,
    reason: DelegationReason,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = TurnState::DelegationTerminated {
        spawning_request_id: uuid(4),
        outcome,
        reason,
        provenance: DelegationProvenance::ParentGoalCommand {
            parent_session_id: uuid(1),
            goal_generation: CanonicalU64::new(2),
            command_id: uuid(7),
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        },
    };
    let encoded = serde_json::to_value(&state)?;
    assert_eq!(serde_json::from_value::<TurnState>(encoded)?, state);

    let frame = ServerFrame::try_new(
        request(1)?,
        ServerMessage::TranscriptTurn {
            turn_id: uuid(1),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state,
        },
    )?;
    assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
    Ok(())
}
