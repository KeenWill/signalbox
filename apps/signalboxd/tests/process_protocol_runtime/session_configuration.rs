//! Session configuration coverage.

use super::*;

pub(crate) fn provider_default_model_settings() -> ModelSettingsSnapshot {
    ModelSettingsSnapshot {
        precedence: ModelSettingsPrecedence {
            per_call: ModelSettingsOverlay::inherit_all(),
            session: ModelSettingsOverlay::inherit_all(),
            profile: ModelSettingsOverlay::inherit_all(),
            global_default: ModelSettingsOverlay::inherit_all(),
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

pub(crate) async fn create_direct_session_with_settings(
    connection: &mut Connection,
    selection_id: CanonicalUuid,
    model_settings: ModelSettingsOverlay,
) -> Result<(CanonicalUuid, ModelSettingsSnapshot), Box<dyn Error>> {
    connection
        .request(
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Direct { selection_id },
                model_settings,
                system_prompt: SystemPromptMember::present(None),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;
    match response_within(connection).await?.message() {
        ServerMessage::SessionCreated {
            session_id,
            model_settings,
        } => Ok((*session_id, *model_settings)),
        message => Err(io::Error::other(format!(
            "unexpected direct create-session response: {message:?}"
        ))
        .into()),
    }
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn create_session_rejects_a_model_absent_from_the_static_mapping()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let unknown_selection = CanonicalUuid::from_uuid(Uuid::from_u128(0xffff));
    connection
        .request(
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: unknown_selection,
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(None),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;

    let response = response_within(&mut connection).await?;
    let ServerMessage::Error { code, .. } = response.message() else {
        panic!("unmapped model must return a protocol error");
    };
    assert_eq!(*code, ErrorCode::InvalidRequest);

    drop(connection);
    runtime.stop().await
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SessionModelSettingsChangedEventFacts {
    pub(crate) session_id: CanonicalUuid,
    pub(crate) prior_defaults_version: u64,
    pub(crate) installed_defaults_version: u64,
    pub(crate) installed_settings: ModelSettingsSnapshot,
    pub(crate) caller_override: ModelSettingsOverlay,
    pub(crate) adjustments: Vec<ModelChangeAdjustment>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SessionDefaultsReplacedFacts {
    pub(crate) defaults_version: CanonicalU64,
    pub(crate) installed_settings: ModelSettingsSnapshot,
}

#[track_caller]
pub(crate) fn session_defaults_replaced_facts(
    message: &ServerMessage,
) -> SessionDefaultsReplacedFacts {
    match message {
        ServerMessage::SessionDefaultsReplaced {
            defaults_version,
            model_settings,
            ..
        } => SessionDefaultsReplacedFacts {
            defaults_version: *defaults_version,
            installed_settings: *model_settings,
        },
        message => panic!("fixture expected defaults-replaced receipt, got {message:?}"),
    }
}

#[track_caller]
pub(crate) fn session_model_settings_changed_event_facts(
    message: &ServerMessage,
) -> SessionModelSettingsChangedEventFacts {
    match message {
        ServerMessage::SessionEvent {
            session_id,
            event:
                SessionEvent::SessionModelSettingsChanged {
                    prior_defaults_version,
                    installed_defaults_version,
                    installed_settings,
                    caller_override,
                    adjustments,
                    ..
                },
            ..
        } => SessionModelSettingsChangedEventFacts {
            session_id: *session_id,
            prior_defaults_version: prior_defaults_version.value(),
            installed_defaults_version: installed_defaults_version.value(),
            installed_settings: *installed_settings,
            caller_override: *caller_override,
            adjustments: adjustments.clone(),
        },
        message => panic!("fixture expected session-settings change event, got {message:?}"),
    }
}

/// one complete replacement request through the durable command boundary and validates catalog
/// input before claiming a new command identity.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_replaces_session_model_defaults() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let replacement_command = command()?;
    let replacement_selection = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let replacement = ClientRequest::ReplaceSessionDefaults {
        command_id: replacement_command,
        session_id,
        expected_defaults_version: CanonicalU64::new(1),
        model_selection: ModelSelection::Direct {
            selection_id: replacement_selection,
        },
        dangerous_tool_auto_approval: false,
        model_settings: ModelSettingsOverlay::inherit_all(),
        system_prompt: SystemPromptMember::present(None),
    };

    connection
        .request_version(ProtocolVersion::One, 2, replacement.clone())
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionDefaultsReplaced {
            session_id,
            defaults_version: CanonicalU64::new(2),
            model_selection: ModelSelection::Direct {
                selection_id: replacement_selection,
            },
            dangerous_tool_auto_approval: false,
            model_settings: provider_default_model_settings(),
            system_prompt: SystemPromptMember::present(None),
        }
    );

    connection
        .request_version(ProtocolVersion::One, 3, replacement)
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionDefaultsReplaced {
            session_id,
            defaults_version: CanonicalU64::new(2),
            model_selection: ModelSelection::Direct {
                selection_id: replacement_selection,
            },
            dangerous_tool_auto_approval: false,
            model_settings: provider_default_model_settings(),
            system_prompt: SystemPromptMember::present(None),
        }
    );

    let unknown_command = command()?;
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ReplaceSessionDefaults {
                command_id: unknown_command,
                session_id,
                expected_defaults_version: CanonicalU64::new(2),
                model_selection: ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(999)),
                },
                dangerous_tool_auto_approval: false,
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    let unknown_claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(unknown_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(unknown_claim_count, 0);

    drop(connection);
    runtime.stop().await
}

/// a prompted session exposes exact current and named defaults epochs and replaces the prompt
/// forward-only with the complete installed echo.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_carries_the_session_system_prompt() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let prompt = SystemPromptText::try_new(String::from("exact review instructions"))
        .expect("test prompt is admissible");
    let selection = CanonicalUuid::from_uuid(Uuid::from_u128(1));

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: selection,
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(Some(prompt.clone())),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;
    let created = session_created_facts(response_within(&mut connection).await?.message());
    let session_id = created.session_id;
    assert_eq!(created.model_settings, provider_default_model_settings());

    connection
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::ReadSessionDefaults {
                session_id,
                defaults_version: None,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionDefaults {
            session_id,
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: selection,
            },
            dangerous_tool_auto_approval: false,
            model_settings: provider_default_model_settings(),
            system_prompt: Some(prompt.clone()),
        }
    );

    // The replacement states the complete successor explicitly,
    // clearing the prompt, and its receipt echoes the complete install.
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: selection,
                },
                dangerous_tool_auto_approval: false,
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionDefaultsReplaced {
            session_id,
            defaults_version: CanonicalU64::new(2),
            model_selection: ModelSelection::Direct {
                selection_id: selection,
            },
            dangerous_tool_auto_approval: false,
            model_settings: provider_default_model_settings(),
            system_prompt: SystemPromptMember::present(None),
        }
    );

    connection
        .request_version(
            ProtocolVersion::One,
            6,
            ClientRequest::ReadSessionDefaults {
                session_id,
                defaults_version: Some(CanonicalU64::new(1)),
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionDefaults {
            session_id,
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: selection,
            },
            dangerous_tool_auto_approval: false,
            model_settings: provider_default_model_settings(),
            system_prompt: Some(prompt),
        }
    );

    connection
        .request_version(
            ProtocolVersion::One,
            7,
            ClientRequest::ReadSessionDefaults {
                session_id,
                defaults_version: Some(CanonicalU64::new(99)),
            },
        )
        .await?;
    assert_eq!(
        protocol_error_code(response_within(&mut connection).await?.message()),
        ErrorCode::NotFound
    );
    connection
        .request_version(
            ProtocolVersion::One,
            8,
            ClientRequest::ReadSessionDefaults {
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(0xdead)),
                defaults_version: None,
            },
        )
        .await?;
    assert_eq!(
        protocol_error_code(response_within(&mut connection).await?.message()),
        ErrorCode::NotFound
    );

    drop(connection);
    runtime.stop().await
}

/// an explicit unsupported replacement value is a typed caller error even when changing models
/// would have adjusted an inherited value.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn model_change_rejects_an_explicit_unsupported_setting() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let requested_reasoning = ReasoningLevel::Low;
    let requested = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(requested_reasoning),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };
    let (session_id, _) = create_direct_session_with_settings(
        &mut connection,
        primary_direct_selection_id(),
        requested,
    )
    .await?;

    connection
        .request(
            2,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: next_direct_selection_id(),
                },
                model_settings: requested,
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let error = response_within(&mut connection).await?.message().clone();

    assert_eq!(protocol_error_code(&error), ErrorCode::Rejected);
    assert_eq!(
        protocol_error_detail(&error),
        Some(RejectionDetail::UnsupportedReasoningLevel {
            selection_id: next_direct_selection_id(),
            requested: requested_reasoning,
        })
    );

    drop(connection);
    runtime.stop().await
}

/// defaults replacement carries the prior session layer across a model change, clears an inherited
/// incompatible value, and emits the exact automatic adjustment as durable follower evidence.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn model_change_clamps_inherited_session_settings() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let requested_reasoning = ReasoningLevel::Low;
    let caller_session_settings = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(requested_reasoning),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };
    let (session_id, created_settings) = create_direct_session_with_settings(
        &mut connection,
        primary_direct_selection_id(),
        caller_session_settings,
    )
    .await?;
    let mut follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 10, session_id).await?;

    connection
        .request(
            2,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: next_direct_selection_id(),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let replacement =
        session_defaults_replaced_facts(response_within(&mut connection).await?.message());
    let defaults_version = replacement.defaults_version;
    let installed_settings = replacement.installed_settings;
    let event =
        session_model_settings_changed_event_facts(response_within(&mut follow).await?.message());

    assert_eq!(
        created_settings.effective.reasoning_level,
        Some(requested_reasoning)
    );
    assert_eq!(defaults_version.value(), 2);
    assert_eq!(installed_settings.effective.reasoning_level, None);
    assert_eq!(
        installed_settings.precedence.session.reasoning_level,
        SettingOverlay::ProviderDefault
    );
    assert_eq!(
        installed_settings.validated_for_selection_id,
        Some(next_direct_selection_id())
    );
    assert_eq!(event.session_id, session_id);
    assert_eq!(event.prior_defaults_version, 1);
    assert_eq!(event.installed_defaults_version, defaults_version.value());
    assert_eq!(event.installed_settings, installed_settings);
    assert_eq!(event.caller_override, ModelSettingsOverlay::inherit_all());
    assert_eq!(
        event.adjustments,
        [ModelChangeAdjustment::ReasoningLevelCleared {
            from: requested_reasoning,
        }]
    );

    drop(follow);
    drop(connection);
    runtime.stop().await
}

/// an equal explicit-creation replay is decided from its durable command before the current
/// deployment revalidates model settings.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn create_session_replays_after_capability_removal() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let command_id = command()?;
    let requested_settings = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::Low),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request(
            1,
            ClientRequest::CreateSession {
                command_id,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
                system_prompt: SystemPromptMember::present(None),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;
    let applied = session_created_facts(response_within(&mut connection).await?.message());
    drop(connection);

    let configuration_without_reasoning =
        MODEL_CONFIGURATION.replace("reasoning_levels = [\"low\"]\n", "");
    let _recovered_turn_count = runtime
        .restart_with_model_configuration(&configuration_without_reasoning)
        .await?;
    let mut replay_connection = Connection::connect(runtime.socket()).await?;
    replay_connection
        .request(
            2,
            ClientRequest::CreateSession {
                command_id,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
                system_prompt: SystemPromptMember::present(None),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;
    let replayed = session_created_facts(response_within(&mut replay_connection).await?.message());

    assert_eq!(replayed, applied);

    drop(replay_connection);
    runtime.stop().await
}

/// an equal defaults-replacement replay returns its durable result before the current deployment
/// revalidates model settings.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn defaults_replacement_replays_after_capability_removal() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let (session_id, _) = create_direct_session_with_settings(
        &mut connection,
        primary_direct_selection_id(),
        ModelSettingsOverlay::inherit_all(),
    )
    .await?;
    let command_id = command()?;
    let requested_settings = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::Low),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };
    connection
        .request(
            2,
            ClientRequest::ReplaceSessionDefaults {
                command_id,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let applied =
        session_defaults_replaced_facts(response_within(&mut connection).await?.message());
    drop(connection);

    let configuration_without_reasoning =
        MODEL_CONFIGURATION.replace("reasoning_levels = [\"low\"]\n", "");
    let _recovered_turn_count = runtime
        .restart_with_model_configuration(&configuration_without_reasoning)
        .await?;
    let mut replay_connection = Connection::connect(runtime.socket()).await?;
    replay_connection
        .request(
            3,
            ClientRequest::ReplaceSessionDefaults {
                command_id,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let replayed =
        session_defaults_replaced_facts(response_within(&mut replay_connection).await?.message());

    assert_eq!(replayed, applied);

    drop(replay_connection);
    runtime.stop().await
}

/// a stale replacement records and replays its authoritative version mismatch before current
/// capability validation can reject settings.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn stale_defaults_replacement_precedes_settings_validation() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let expected_version = CanonicalU64::new(1);
    let current_version = CanonicalU64::new(2);
    let (session_id, _) = create_direct_session_with_settings(
        &mut connection,
        primary_direct_selection_id(),
        ModelSettingsOverlay::inherit_all(),
    )
    .await?;
    connection
        .request(
            2,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: expected_version,
                model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let applied =
        session_defaults_replaced_facts(response_within(&mut connection).await?.message());
    let stale = ClientRequest::ReplaceSessionDefaults {
        command_id: command()?,
        session_id,
        expected_defaults_version: expected_version,
        model_selection: ModelSelection::Direct {
            selection_id: next_direct_selection_id(),
        },
        model_settings: ModelSettingsOverlay {
            reasoning_level: SettingOverlay::Value(ReasoningLevel::Low),
            fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
            service_tier: SettingOverlay::Inherit,
        },
        dangerous_tool_auto_approval: false,
        system_prompt: SystemPromptMember::present(None),
    };
    let expected_rejection = RejectionDetail::DefaultsVersionMismatch {
        session_id,
        expected: expected_version,
        current: current_version,
    };

    connection.request(3, stale.clone()).await?;
    let first = rejected_detail(response_within(&mut connection).await?.message());
    connection.request(4, stale).await?;
    let replayed = rejected_detail(response_within(&mut connection).await?.message());

    assert_eq!(applied.defaults_version, current_version);
    assert_eq!(first, expected_rejection);
    assert_eq!(replayed, expected_rejection);

    drop(connection);
    runtime.stop().await
}

/// an unknown replacement selection is the read-only catalog error even when the same frame names
/// an epoch the session has not reached, and it leaves the command identity available for the
/// corrected request.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn unknown_replacement_model_precedes_defaults_version_mismatch() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let (session_id, _) = create_direct_session_with_settings(
        &mut connection,
        primary_direct_selection_id(),
        ModelSettingsOverlay::inherit_all(),
    )
    .await?;
    let command_id = command()?;
    let unknown_selection = CanonicalUuid::from_uuid(Uuid::from_u128(0xffff));

    connection
        .request(
            2,
            ClientRequest::ReplaceSessionDefaults {
                command_id,
                session_id,
                expected_defaults_version: CanonicalU64::new(2),
                model_selection: ModelSelection::Direct {
                    selection_id: unknown_selection,
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let unknown = response_within(&mut connection).await?.message().clone();

    assert_eq!(protocol_error_code(&unknown), ErrorCode::InvalidRequest);
    assert_eq!(protocol_error_detail(&unknown), None);

    connection
        .request(
            3,
            ClientRequest::ReplaceSessionDefaults {
                command_id,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let corrected =
        session_defaults_replaced_facts(response_within(&mut connection).await?.message());

    assert_eq!(corrected.defaults_version, CanonicalU64::new(2));

    drop(connection);
    runtime.stop().await
}

/// an absent session reaches the durable replacement boundary before compatibility validation and
/// replays its recorded terminal result.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn absent_defaults_replacement_precedes_settings_validation() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let absent_session_id = CanonicalUuid::from_uuid(Uuid::from_u128(0x3701));
    let replacement = ClientRequest::ReplaceSessionDefaults {
        command_id: command()?,
        session_id: absent_session_id,
        expected_defaults_version: CanonicalU64::new(1),
        model_selection: ModelSelection::Direct {
            selection_id: next_direct_selection_id(),
        },
        model_settings: low_reasoning_override(),
        dangerous_tool_auto_approval: false,
        system_prompt: SystemPromptMember::present(None),
    };
    let expected = RejectionDetail::SessionNotFound {
        session_id: absent_session_id,
    };

    connection.request(1, replacement.clone()).await?;
    let first = rejected_detail(response_within(&mut connection).await?.message());
    connection.request(2, replacement).await?;
    let replayed = rejected_detail(response_within(&mut connection).await?.message());

    assert_eq!(first, expected);
    assert_eq!(replayed, expected);

    drop(connection);
    runtime.stop().await
}
