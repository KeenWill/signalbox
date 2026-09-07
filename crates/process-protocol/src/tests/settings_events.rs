//! Settings events protocol tests.

use super::support::*;
use crate::settings::validate_adjustments;
use crate::*;

#[test]
fn turn_settings_event_round_trip_preserves_override_provenance()
-> Result<(), Box<dyn std::error::Error>> {
    let message = ServerMessage::SessionEvent {
        cursor: CanonicalU64::new(9),
        session_id: uuid(1),
        event: SessionEvent::TurnModelSettingsResolved {
            accepted_input_id: uuid(2),
            turn_id: uuid(3),
            defaults_version: CanonicalU64::new(7),
            requested_model: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            selected_direct_id: uuid(4),
            per_call_override: settings_snapshot_fixture().precedence.per_call,
            settings: settings_snapshot_fixture(),
            adjusted_from_selection_id: None,
            adjustments: Vec::new(),
        },
    };

    let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(42)?, message)?;
    let encoded = encode_server_line(&frame)?;
    let decoded = decode_server_line(&encoded)?;

    assert_eq!(decoded, frame);
    Ok(())
}

/// a late follower's authoritative turn projection
/// carries the same complete frozen settings evidence as the durable event.
#[test]
fn transcript_turn_round_trips_frozen_settings() -> Result<(), Box<dyn std::error::Error>> {
    let settings = settings_snapshot_fixture();
    let message = ServerMessage::TranscriptTurn {
        turn_id: uuid(3),
        acceptance_position: CanonicalU64::new(1),
        model_settings: Some(TurnModelSettingsSnapshot {
            turn_id: uuid(3),
            accepted_input_id: uuid(2),
            defaults_version: CanonicalU64::new(7),
            requested_model: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            selected_direct_id: uuid(4),
            per_call_override: settings.precedence.per_call,
            settings,
            adjusted_from_selection_id: None,
            adjustments: Vec::new(),
        }),
        state: TurnState::Queued {
            accepted_input_id: uuid(2),
            content: UserInputContent::text("settings-aware turn".to_owned()),
        },
    };
    let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(43)?, message)?;
    let encoded = encode_server_line(&frame)?;

    assert_eq!(decode_server_line(&encoded)?, frame);
    Ok(())
}

/// queued user content is validated before a server frame can be
/// encoded, including when no model-settings snapshot is present.
#[test]
fn transcript_turn_rejects_invalid_queued_content_before_encoding() {
    let result = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Queued {
                accepted_input_id: uuid(2),
                content: UserInputContent::from_parts(Vec::new()),
            },
        },
    );

    assert_eq!(result, Err(FrameValidationError::UserContentShape));
}

/// queued turn settings evidence belongs to the accepted input
/// named by the authoritative queued state.
#[test]
fn transcript_turn_rejects_settings_for_another_queued_input() {
    let settings = settings_snapshot_fixture();
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: Some(TurnModelSettingsSnapshot {
                turn_id: uuid(3),
                accepted_input_id: uuid(5),
                defaults_version: CanonicalU64::new(7),
                requested_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                selected_direct_id: uuid(4),
                per_call_override: settings.precedence.per_call,
                settings,
                adjusted_from_selection_id: None,
                adjustments: Vec::new(),
            }),
            state: TurnState::Queued {
                accepted_input_id: uuid(2),
                content: UserInputContent::text("settings-aware turn".to_owned()),
            },
        },
    )
    .expect_err("queued settings must name the queued accepted input");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

/// terminal turn settings evidence belongs to the turn named by
/// the authoritative transcript projection.
#[test]
fn transcript_turn_rejects_settings_for_another_terminal_turn() {
    let settings = settings_snapshot_fixture();
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: Some(TurnModelSettingsSnapshot {
                turn_id: uuid(5),
                accepted_input_id: uuid(2),
                defaults_version: CanonicalU64::new(7),
                requested_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                selected_direct_id: uuid(4),
                per_call_override: settings.precedence.per_call,
                settings,
                adjusted_from_selection_id: None,
                adjustments: Vec::new(),
            }),
            state: TurnState::Completed {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: uuid(7),
                terminal_model_call_id: uuid(8),
            },
        },
    )
    .expect_err("terminal settings must name the projected turn");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

/// required-nullable turn settings cannot be omitted.
#[test]
fn transcript_turn_requires_model_settings_member() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"queued","accepted_input_id":"00000000-0000-0000-0000-000000000002","content":[{"type":"text","text":"queued request"}]}}}"#,
    );
}

/// complete settings snapshots cannot contradict their retained
/// precedence provenance.
#[test]
fn model_settings_snapshot_rejects_inconsistent_effective_values() {
    let mut model_settings = session_settings_snapshot_fixture();
    model_settings.effective.reasoning_level = Some(ReasoningLevel::Low);
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionCreated {
            session_id: uuid(1),
            model_settings,
        },
    )
    .expect_err("effective settings must resolve from retained provenance");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

/// only the exact all-inherit provider-default snapshot is
/// model-independent.
#[test]
fn nondefault_settings_snapshot_requires_validation_identity() {
    let mut model_settings = session_settings_snapshot_fixture();
    model_settings.validated_for_selection_id = None;

    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionCreated {
            session_id: uuid(1),
            model_settings,
        },
    )
    .expect_err("nondefault settings require their validating direct selection");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

/// durable defaults snapshots cannot retain a per-call layer.
#[test]
fn defaults_snapshot_rejects_per_call_settings() {
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionCreated {
            session_id: uuid(1),
            model_settings: settings_snapshot_fixture(),
        },
    )
    .expect_err("defaults cannot retain an origin-only per-call contribution");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

/// the separately reported per-call contribution must equal the
/// retained precedence layer.
#[test]
fn turn_settings_event_rejects_crosswired_per_call_override() {
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: uuid(2),
                turn_id: uuid(3),
                defaults_version: CanonicalU64::new(1),
                requested_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                selected_direct_id: uuid(4),
                per_call_override: ModelSettingsOverlay::inherit_all(),
                settings: settings_snapshot_fixture(),
                adjusted_from_selection_id: None,
                adjustments: Vec::new(),
            },
        },
    )
    .expect_err("event provenance must match the sealed per-call layer");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

/// adjustments require a distinct prior direct validation identity.
#[test]
fn turn_settings_event_rejects_unchanged_adjustment_source() {
    let mut settings = settings_snapshot_fixture();
    settings.precedence.session = settings.precedence.per_call;
    settings.precedence.per_call = ModelSettingsOverlay::inherit_all();
    settings.reasoning_source = Some(ModelSettingSource::Session);
    settings.service_tier_source = Some(ModelSettingSource::Session);
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: uuid(2),
                turn_id: uuid(3),
                defaults_version: CanonicalU64::new(1),
                requested_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                selected_direct_id: uuid(4),
                per_call_override: ModelSettingsOverlay::inherit_all(),
                settings,
                adjusted_from_selection_id: Some(uuid(4)),
                adjustments: vec![ModelChangeAdjustment::ReasoningLevelClamped {
                    from: ReasoningLevel::XHigh,
                    to: ReasoningLevel::High,
                }],
            },
        },
    )
    .expect_err("the selected model cannot also be the adjustment source");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

/// a distinct prior direct selection authenticates automatic
/// model-change adjustment evidence for the frozen turn.
#[test]
fn turn_settings_event_accepts_distinct_adjustment_source() {
    let mut settings = settings_snapshot_fixture();
    settings.precedence.session = settings.precedence.per_call;
    settings.precedence.per_call = ModelSettingsOverlay::inherit_all();
    settings.reasoning_source = Some(ModelSettingSource::Session);
    settings.service_tier_source = Some(ModelSettingSource::Session);
    let result = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: uuid(2),
                turn_id: uuid(3),
                defaults_version: CanonicalU64::new(1),
                requested_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                selected_direct_id: uuid(4),
                per_call_override: ModelSettingsOverlay::inherit_all(),
                settings,
                adjusted_from_selection_id: Some(uuid(5)),
                adjustments: vec![ModelChangeAdjustment::ReasoningLevelClamped {
                    from: ReasoningLevel::XHigh,
                    to: ReasoningLevel::High,
                }],
            },
        },
    );

    assert!(result.is_ok());
}

/// caller and adjustment evidence must derive the exact installed
/// defaults snapshot.
#[test]
fn settings_change_event_rejects_unrelated_installed_snapshot() {
    let prior_settings = provider_default_settings_snapshot_fixture();
    let installed_settings = session_settings_snapshot_fixture();
    let model = ModelSelection::Direct {
        selection_id: uuid(4),
    };

    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::SessionModelSettingsChanged {
                command_id: command(2).expect("fixture command identity is admitted"),
                prior_defaults_version: CanonicalU64::new(1),
                installed_defaults_version: CanonicalU64::new(2),
                prior_model: model,
                installed_model: model,
                prior_settings,
                installed_settings,
                caller_override: ModelSettingsOverlay::inherit_all(),
                adjustments: Vec::new(),
            },
        },
    )
    .expect_err("an all-inherit caller cannot install an unrelated session layer");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

/// an automatic model-change adjustment cannot rewrite a value
/// explicitly supplied by the caller of that same defaults replacement.
#[test]
fn settings_change_rejects_adjustment_to_caller_explicit_value() {
    let prior_settings = provider_default_settings_snapshot_fixture();
    let mut installed_settings = provider_default_settings_snapshot_fixture();
    installed_settings.precedence.session.reasoning_level =
        SettingOverlay::Value(ReasoningLevel::Low);
    installed_settings.effective.reasoning_level = Some(ReasoningLevel::Low);
    installed_settings.reasoning_source = Some(ModelSettingSource::Session);
    installed_settings.validated_for_selection_id = Some(uuid(4));
    let caller_override = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::High),
        fast_mode: FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };

    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::SessionModelSettingsChanged {
                command_id: command(2).expect("fixture command identity is admitted"),
                prior_defaults_version: CanonicalU64::new(1),
                installed_defaults_version: CanonicalU64::new(2),
                prior_model: ModelSelection::Direct {
                    selection_id: uuid(3),
                },
                installed_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                prior_settings,
                installed_settings,
                caller_override,
                adjustments: vec![ModelChangeAdjustment::ReasoningLevelClamped {
                    from: ReasoningLevel::High,
                    to: ReasoningLevel::Low,
                }],
            },
        },
    )
    .expect_err("caller-owned settings cannot be adjusted automatically");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

/// defaults reads bind a direct model to the snapshot validation
/// identity.
#[test]
fn defaults_read_rejects_crosswired_direct_settings() {
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionDefaults {
            session_id: uuid(1),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(5),
            },
            model_settings: session_settings_snapshot_fixture(),
            dangerous_tool_auto_approval: false,
            system_prompt: None,
        },
    )
    .expect_err("direct defaults require settings validated for that selection");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

/// settings-change events reject both reserved command
/// identities during wire decoding.
#[test]
fn settings_change_event_rejects_command_sentinels() {
    let nil = format!(
        "{{\"version\":1,\"request_id\":\"1\",\"message\":{{\"type\":\"session_event\",\"cursor\":\"1\",\"session_id\":\"00000000-0000-0000-0000-000000000001\",\"event\":{{\"type\":\"session_model_settings_changed\",\"command_id\":\"00000000-0000-0000-0000-000000000000\",\"prior_defaults_version\":\"1\",\"installed_defaults_version\":\"2\",\"prior_model\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"}},\"installed_model\":{{\"kind\":\"alias\",\"alias_id\":\"00000000-0000-0000-0000-000000000005\"}},\"prior_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON},\"installed_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON},\"caller_override\":{{\"reasoning_level\":{{\"kind\":\"inherit\"}},\"fast_mode\":{{\"kind\":\"inherit\"}},\"service_tier\":{{\"kind\":\"inherit\"}}}},\"adjustments\":[]}}}}}}"
    );
    let all_ones = nil.replace(
        "00000000-0000-0000-0000-000000000000",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
    );

    assert_server_malformed(&nil);
    assert_server_malformed(&all_ones);
}

#[test]
fn fast_mode_overlay_rejects_provider_default_on_the_wire() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"1","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"model_settings":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"provider_default"},"service_tier":{"kind":"inherit"}},"system_prompt":null}}"#,
    );
}

/// steering inherits its source turn and cannot carry an
/// independent settings contribution.
#[test]
fn steering_rejects_a_model_settings_override() {
    let mut model_settings = ModelSettingsOverlay::inherit_all();
    model_settings.reasoning_level = SettingOverlay::Value(ReasoningLevel::High);
    let error = ClientFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ClientRequest::SubmitInput {
            command_id: command(1).expect("fixture command identity is admitted"),
            session_id: uuid(2),
            content: UserInputContent::text(String::from("steer")),
            expected_defaults_version: None,
            model_settings,
            delivery: Some(InputDelivery::Steer {
                expected_active_turn_id: uuid(3),
            }),
        },
    )
    .expect_err("steering cannot override its source turn settings");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

#[test]
fn capability_item_rejects_noncanonical_reasoning_order() -> Result<(), Box<dyn std::error::Error>>
{
    let message = ServerMessage::ModelCapabilityItem {
        selection_id: uuid(4),
        capabilities: ModelCapabilities {
            reasoning_levels: vec![ReasoningLevel::High, ReasoningLevel::Low],
            fast_mode_supported: false,
            service_tiers: Vec::new(),
        },
    };

    let error = ServerFrame::try_new_for_version(ProtocolVersion::One, request(43)?, message)
        .expect_err("capability sets use canonical ascending wire order");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
    Ok(())
}

#[test]
fn nested_model_setting_tags_reject_unknown_members() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"1","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"model_settings":{"reasoning_level":{"kind":"inherit","extra":1},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"system_prompt":null}}"#,
    );
    assert_client_malformed(
        r#"{"version":1,"request_id":"1","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"model_settings":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit","extra":1},"service_tier":{"kind":"inherit"}},"system_prompt":null}}"#,
    );
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"model_capability_item","selection_id":"00000000-0000-0000-0000-000000000004","capabilities":{"reasoning_levels":[],"fast_mode_supported":false,"service_tiers":[{"provider":"open_ai","value":"priority","extra":1}]}}}"#,
    );
}

#[test]
fn settings_change_event_rejects_zero_prior_version() {
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::SessionModelSettingsChanged {
                command_id: command(2).expect("fixture command identity is admitted"),
                prior_defaults_version: CanonicalU64::new(0),
                installed_defaults_version: CanonicalU64::new(1),
                prior_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                installed_model: ModelSelection::Alias { alias_id: uuid(5) },
                prior_settings: provider_default_settings_snapshot_fixture(),
                installed_settings: provider_default_settings_snapshot_fixture(),
                caller_override: ModelSettingsOverlay::inherit_all(),
                adjustments: Vec::new(),
            },
        },
    )
    .expect_err("a settings change cannot precede the initial defaults epoch");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

#[test]
fn settings_change_event_rejects_a_no_op() {
    let model = ModelSelection::Direct {
        selection_id: uuid(4),
    };
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::SessionModelSettingsChanged {
                command_id: command(2).expect("fixture command identity is admitted"),
                prior_defaults_version: CanonicalU64::new(1),
                installed_defaults_version: CanonicalU64::new(2),
                prior_model: model,
                installed_model: model,
                prior_settings: provider_default_settings_snapshot_fixture(),
                installed_settings: provider_default_settings_snapshot_fixture(),
                caller_override: ModelSettingsOverlay::inherit_all(),
                adjustments: Vec::new(),
            },
        },
    )
    .expect_err("a durable settings-change event must record an actual change");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

#[test]
fn turn_settings_event_rejects_a_mismatched_direct_selection() {
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: uuid(2),
                turn_id: uuid(3),
                defaults_version: CanonicalU64::new(1),
                requested_model: ModelSelection::Direct {
                    selection_id: uuid(5),
                },
                selected_direct_id: uuid(4),
                per_call_override: settings_snapshot_fixture().precedence.per_call,
                settings: settings_snapshot_fixture(),
                adjusted_from_selection_id: None,
                adjustments: Vec::new(),
            },
        },
    )
    .expect_err("only an alias can resolve to a distinct direct selection");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

#[test]
fn turn_settings_event_admits_model_independent_provider_defaults() {
    let result = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: uuid(2),
                turn_id: uuid(3),
                defaults_version: CanonicalU64::new(1),
                requested_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                selected_direct_id: uuid(4),
                per_call_override: ModelSettingsOverlay::inherit_all(),
                settings: provider_default_settings_snapshot_fixture(),
                adjusted_from_selection_id: None,
                adjustments: Vec::new(),
            },
        },
    );

    assert!(result.is_ok());
}

#[test]
fn turn_settings_event_requires_validation_for_non_default_settings() {
    let mut settings = settings_snapshot_fixture();
    settings.validated_for_selection_id = None;
    let error = ServerFrame::try_new(
        RequestId::try_new(1).expect("fixture request identity is admitted"),
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: uuid(2),
                turn_id: uuid(3),
                defaults_version: CanonicalU64::new(1),
                requested_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                selected_direct_id: uuid(4),
                per_call_override: settings.precedence.per_call,
                settings,
                adjusted_from_selection_id: None,
                adjustments: Vec::new(),
            },
        },
    )
    .expect_err("non-default settings require exact validation provenance");

    assert_eq!(error, FrameValidationError::ModelSettingsShape);
}

#[test]
fn adjustment_inventory_rejects_duplicates_order_and_excess() {
    let duplicate = vec![
        ModelChangeAdjustment::ReasoningLevelClamped {
            from: ReasoningLevel::XHigh,
            to: ReasoningLevel::High,
        },
        ModelChangeAdjustment::ReasoningLevelCleared {
            from: ReasoningLevel::High,
        },
    ];
    let reversed = vec![
        ModelChangeAdjustment::FastModeDisabled {},
        ModelChangeAdjustment::ReasoningLevelCleared {
            from: ReasoningLevel::High,
        },
    ];
    let excessive = vec![
        ModelChangeAdjustment::ReasoningLevelCleared {
            from: ReasoningLevel::High,
        },
        ModelChangeAdjustment::FastModeDisabled {},
        ModelChangeAdjustment::ServiceTierCleared {
            from: ServiceTier::OpenAi(OpenAiServiceTier::Priority),
        },
        ModelChangeAdjustment::ServiceTierCleared {
            from: ServiceTier::OpenAi(OpenAiServiceTier::Flex),
        },
    ];

    assert_eq!(
        validate_adjustments(&duplicate),
        Err(FrameValidationError::ModelSettingsShape)
    );
    assert_eq!(
        validate_adjustments(&reversed),
        Err(FrameValidationError::ModelSettingsShape)
    );
    assert_eq!(
        validate_adjustments(&excessive),
        Err(FrameValidationError::ModelSettingsShape)
    );
}

#[test]
fn runner_state_transition_round_trips_complete_placement_facts()
-> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(2),
            session_id: uuid(3),
            event: SessionEvent::RunnerStateTransition {
                runner_id: uuid(4),
                placement_revision: RunnerPlacementRevision::try_new(5)
                    .expect("the fixture placement revision is positive"),
                sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
                working_directory: Some(
                    RunnerWorkingDirectory::try_new(String::from("workspace/project"))
                        .expect("the fixture working directory is valid"),
                ),
                state: RunnerStateTransitionState::WorkingDirectoryChanged,
            },
        },
        r#"{"type":"session_event","cursor":"2","session_id":"00000000-0000-0000-0000-000000000003","event":{"type":"runner_state_transition","runner_id":"00000000-0000-0000-0000-000000000004","placement_revision":"5","sandbox_profile":"workspace-restricted","working_directory":"workspace/project","state":"working_directory_changed"}}"#,
    )?;
    Ok(())
}

#[test]
fn runner_placed_session_summary_round_trips_complete_projection()
-> Result<(), Box<dyn std::error::Error>> {
    let runner = RunnerProjection::try_new(
        RunnerProjectionSelector::CapabilityClass {
            name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))?,
        },
        Some(uuid(4)),
        RunnerPlacementRevision::try_new(3).expect("the fixture placement revision is positive"),
        RunnerSandboxProfile::WorkspaceRestricted,
        Some(RunnerCredentialProfileName::try_new(String::from(
            "readonly",
        ))?),
        Some(RunnerRepositoryKey::try_new(String::from("primary"))?),
        Some(RunnerWorkingDirectory::try_new(String::from(
            "workspace/project",
        ))?),
        None,
        RunnerProjectionState::RunnerLost,
    )?;

    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::SessionSummary {
            session_id: uuid(2),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Alias { alias_id: uuid(3) },
            placement_version: CanonicalU64::new(1),
            placement: crate::SessionPlacement::Pathless {},
            runner: Some(runner),
        },
        r#"{"type":"session_summary","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"1","model_selection":{"kind":"alias","alias_id":"00000000-0000-0000-0000-000000000003"},"placement_version":"1","placement":{"kind":"pathless"},"runner":{"selector":{"type":"capability_class","name":"linux.workspace"},"runner_id":"00000000-0000-0000-0000-000000000004","placement_revision":"3","sandbox_profile":"workspace-restricted","credential_profile":"readonly","repository":"primary","working_directory":"workspace/project","connection_health":null,"state":"runner_lost"}}"#,
    )?;
    Ok(())
}

#[test]
fn session_summary_rejects_an_omitted_required_nullable_runner() {
    let encoded = br#"{"version":1,"request_id":"1","message":{"type":"session_summary","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"1","model_selection":{"kind":"alias","alias_id":"00000000-0000-0000-0000-000000000003"},"placement_version":"1","placement":{"kind":"pathless"}}}
"#;

    assert!(decode_server_line(encoded).is_err());
}

#[test]
fn runner_state_transition_revision_rejects_zero_at_construction_and_decode() {
    assert_eq!(RunnerPlacementRevision::try_new(0), None);
    assert!(serde_json::from_str::<RunnerPlacementRevision>(r#""0""#).is_err());
}

#[test]
fn runner_working_directory_rejects_every_invalid_wire_shape() {
    assert_eq!(
        RunnerWorkingDirectory::try_new(String::new()),
        Err(crate::CanonicalValueError::RunnerWorkingDirectory)
    );
    assert_eq!(
        RunnerWorkingDirectory::try_new(String::from("bad\0path")),
        Err(crate::CanonicalValueError::RunnerWorkingDirectory)
    );
    assert_eq!(
        RunnerWorkingDirectory::try_new("x".repeat(RunnerWorkingDirectory::MAX_UTF8_BYTES + 1)),
        Err(crate::CanonicalValueError::RunnerWorkingDirectory)
    );
    assert!(serde_json::from_str::<RunnerWorkingDirectory>(r#"""#).is_err());
}
