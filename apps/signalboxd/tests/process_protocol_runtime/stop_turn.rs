//! Stop turn coverage.

use super::*;

/// Activates the session's queued turn, checkpoints its initial model call,
/// and authorizes its send, so the call is durably issued with no terminal
/// observation. Returns the repository and the authorized call for a later
/// observation binding.
pub(crate) async fn authorize_issued_model_call(
    pool: &PgPool,
    session_id: CanonicalUuid,
) -> Result<
    (
        PostgresModelCallRepository,
        Box<signalbox_domain::AuthorizedModelCall>,
        ModelCallId,
    ),
    Box<dyn Error>,
> {
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(pool, session).await?;
    let targets = support::parse_model_configuration(MODEL_CONFIGURATION)?.target_catalog();
    let calls = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("turn-control-fixture"),
    );
    let call = ModelCallId::from_uuid(Uuid::now_v7());
    let PrepareInitialModelCallOutcome::Checkpointed(_) = calls
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    TurnId::from_uuid(Uuid::now_v7()),
                )
            },
        )
        .await?
    else {
        return Err(io::Error::other("the fixture call must checkpoint").into());
    };
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        calls.authorize_send(session, call).await?
    else {
        return Err(io::Error::other("the fixture call must authorize send").into());
    };
    Ok((calls, authorized, call))
}

#[track_caller]
pub(crate) fn cancellation_marker_count(
    messages: &[ServerMessage],
    cancelled_turn: CanonicalUuid,
) -> usize {
    messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ServerMessage::TranscriptEntry {
                    entry: TranscriptEntry::TurnCancelled { turn_id },
                    ..
                } if *turn_id == cancelled_turn
            )
        })
        .count()
}

#[track_caller]
pub(crate) fn tool_use_entry_names(
    messages: &[ServerMessage],
    request: CanonicalUuid,
) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            ServerMessage::TranscriptEntry {
                entry:
                    TranscriptEntry::AssistantToolUse {
                        tool_request_id,
                        tool_name,
                        ..
                    },
                ..
            } if *tool_request_id == request => Some(tool_name.clone()),
            _ => None,
        })
        .collect()
}

#[track_caller]
pub(crate) fn tool_denied_entry_count(messages: &[ServerMessage], request: CanonicalUuid) -> usize {
    messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ServerMessage::TranscriptEntry {
                    entry: TranscriptEntry::ToolDenied {
                        tool_request_id,
                        ..
                    },
                    ..
                } if *tool_request_id == request
            )
        })
        .count()
}

/// the stop verb applies the accepted interrupt treatment — a running turn with no prepared call
/// cancels directly through the existing lifecycle while the stop's content becomes the queued
/// immediate successor.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn stop_turn_cancels_the_activated_turn_and_queues_its_successor()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, stopped_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    activate_turn(&runtime.pool, SessionId::from_uuid(session_id.into_uuid())).await?;

    let successor_content = String::from("continue after the stop");
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: UserInputContent::text(successor_content.clone()),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;
    assert_ne!(successor_turn_id, stopped_turn_id);

    let messages = read_transcript_messages(&mut connection, 4, session_id).await?;
    assert!(matches!(
        turn_state_of(&messages, stopped_turn_id),
        TurnState::Cancelled {
            terminal_model_call_id: None,
            ..
        }
    ));
    let TurnState::Queued { content, .. } = turn_state_of(&messages, successor_turn_id) else {
        panic!("fixture expected queued successor turn");
    };
    assert_eq!(content.single_text(), Some(successor_content.as_str()));
    assert_eq!(cancellation_marker_count(&messages, stopped_turn_id), 1);

    drop(connection);
    runtime.stop().await
}

/// stopping an issued call records the durable cancellation request and retains the slot for
/// lifecycle closure, and a distinct second stop is refused with the exact prior stop authority
/// named.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn stop_turn_requests_cancellation_of_an_issued_call_exactly_once()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, stopped_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let (_, _, issued_call) =
        Box::pin(authorize_issued_model_call(&runtime.pool, session_id)).await?;
    let first_stop_command = command()?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopTurn {
                command_id: first_stop_command,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: UserInputContent::text(String::from("continue after the stop")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;
    assert_ne!(successor_turn_id, stopped_turn_id);

    let messages = read_transcript_messages(&mut connection, 4, session_id).await?;
    let TurnState::ActiveRunning {
        current_model_call: Some(call),
        ..
    } = turn_state_of(&messages, stopped_turn_id)
    else {
        panic!("fixture expected stopped turn with an issued model call");
    };
    assert_eq!(call.model_call_id().into_uuid(), issued_call.into_uuid());
    assert_eq!(
        call.state(),
        CurrentModelCallState::CancellationRequested {}
    );
    assert!(matches!(
        turn_state_of(&messages, successor_turn_id),
        TurnState::Queued { .. }
    ));

    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: UserInputContent::text(String::from("a second distinct stop")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::InterruptAlreadyApplied {
            session_id,
            active_turn_id: stopped_turn_id,
            existing_command_id: CanonicalUuid::from_uuid(first_stop_command.into_uuid()),
        }
    );

    drop(connection);
    runtime.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn lifecycle_closure_retransmission_after_settlement_issues_no_second_interrupt()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, live_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let (calls, issued, _) =
        Box::pin(authorize_issued_model_call(&runtime.pool, session_id)).await?;
    let lifecycle_command = command()?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopSession {
                command_id: lifecycle_command,
                session_id,
                sticky: true,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        )
        .await?;
    let first = response_within(&mut connection).await?;
    let cancellation = issued
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Cancelled);
    let terminal = calls
        .apply_terminal_observation(
            SessionId::from_uuid(session_id.into_uuid()),
            cancellation,
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(terminal, ModelCallTerminalOutcome::Cancelled(_)));
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::StopSession {
                command_id: lifecycle_command,
                session_id,
                sticky: true,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        )
        .await?;
    let replay = response_within(&mut connection).await?;
    let expected = ServerMessage::SessionLifecycleCommandApplied {
        session_id,
        effect: SessionLifecycleEffect::ClosurePending { live_turn_id },
    };
    let applied_core_interrupts: (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT command.command_id)
           FROM submit_input_command AS command
           JOIN durable_command AS envelope USING (command_id)
          WHERE command.session_id = $1
            AND command.delivery_kind = 'interrupt'
            AND command.actor_kind = 'core'
            AND envelope.issuer_kind = 'core'",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;

    assert_eq!(first.message(), &expected);
    assert_eq!(applied_core_interrupts, (1, 1));
    assert_eq!(replay.message(), &expected);

    drop(connection);
    runtime.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn lifecycle_closure_interrupt_does_not_resolve_a_retired_session_model()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, live_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let _issued = Box::pin(authorize_issued_model_call(&runtime.pool, session_id)).await?;
    drop(connection);

    let retired_model_definition = r#"[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000003"
model_family = "anthropic"
provider_model = "fixture-model"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["low"]

"#;
    let retired_alias_definitions = r#"[[aliases]]
alias_id = "00000000-0000-0000-0000-000000000002"
selection_id = "00000000-0000-0000-0000-000000000001"

[[aliases]]
alias_id = "7fde05bc-b4c3-44f7-8a87-748814c80191"
selection_id = "00000000-0000-0000-0000-000000000001"

[[aliases]]
alias_id = "540ce009-c2ec-4a04-b823-c411ea189778"
selection_id = "00000000-0000-0000-0000-000000000001"
"#;
    let configuration_without_model = MODEL_CONFIGURATION
        .replacen(retired_model_definition, "", 1)
        .replacen(retired_alias_definitions, "", 1);
    let _recovered_turn_count = runtime
        .restart_with_templates(
            &configuration_without_model,
            SessionTemplateConfiguration::default(),
        )
        .await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopSession {
                command_id: command()?,
                session_id,
                sticky: true,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionLifecycleCommandApplied {
            session_id,
            effect: SessionLifecycleEffect::ClosurePending { live_turn_id },
        }
    );
    let closure_state: (String, Option<String>, i64) = sqlx::query_as(
        "SELECT lifecycle.state_kind, lifecycle.pending_terminal_outcome_kind,
                count(command.command_id)
           FROM session_lifecycle AS lifecycle
           LEFT JOIN submit_input_command AS command
             ON command.session_id = lifecycle.session_id
            AND command.delivery_kind = 'interrupt'
            AND command.actor_kind = 'core'
            AND command.model_override_kind = 'replace_with'
            AND command.replacement_model_kind = 'direct'
          WHERE lifecycle.session_id = $1
          GROUP BY lifecycle.state_kind, lifecycle.pending_terminal_outcome_kind",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(closure_state, (String::from("terminal"), None, 1));

    drop(connection);
    runtime.stop().await
}

/// every stop refusal is a recorded typed rejection — an empty session records `no_active_turn` and
/// a stale expected turn records `active_turn_mismatch`.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn stop_turn_refusals_are_typed_and_exact() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let unstarted_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xC1));

    connection
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: unstarted_turn_id,
                content: UserInputContent::text(String::from("names no active turn")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::NoActiveTurn {
            session_id,
            expected_active_turn_id: unstarted_turn_id,
        }
    );

    let (_, active_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    activate_turn(&runtime.pool, SessionId::from_uuid(session_id.into_uuid())).await?;
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: unstarted_turn_id,
                content: UserInputContent::text(String::from("names a stale turn")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ActiveTurnMismatch {
            session_id,
            expected_active_turn_id: unstarted_turn_id,
            active_turn_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// an equal stop replay returns its recorded successor, never a
/// second interrupt or a refusal.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn stop_turn_replays_its_recorded_successor() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, stopped_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    activate_turn(&runtime.pool, SessionId::from_uuid(session_id.into_uuid())).await?;

    let decision = ClientRequest::StopTurn {
        command_id: command()?,
        session_id,
        expected_active_turn_id: stopped_turn_id,
        content: UserInputContent::text(String::from("continue after the stop")),
        expected_defaults_version: CanonicalU64::new(1),
        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        model_settings: ModelSettingsOverlay::inherit_all(),
    };
    connection
        .request_version(ProtocolVersion::One, 3, decision.clone())
        .await?;
    let recorded = response_within(&mut connection).await?.message().clone();
    assert!(
        matches!(&recorded, ServerMessage::InputSubmitted {
        session_id: received_session,
        acceptance_position,
        termination: Some(signalbox_process_protocol::TerminationReceipt {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            descendant_count,
        }), ..
    } if *received_session == session_id && acceptance_position.value() == 2 && descendant_count.value() == 0),
        "{recorded:?}"
    );

    connection
        .request_version(ProtocolVersion::One, 4, decision)
        .await?;
    let replayed = response_within(&mut connection).await?.message().clone();
    assert_eq!(
        replayed, recorded,
        "an equal stop retry returns its recorded successor, scope, and count"
    );

    drop(connection);
    runtime.stop().await
}

/// stopping a turn records the explicit per-call contribution with the successor origin instead of
/// dropping it at the daemon boundary.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn stop_turn_records_its_per_call_model_settings() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, stopped_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    activate_turn(&runtime.pool, SessionId::from_uuid(session_id.into_uuid())).await?;
    let requested = low_reasoning_override();

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: UserInputContent::text(String::from("continue with deliberate reasoning")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: requested,
            },
        )
        .await?;
    let settings = accepted_successor_model_settings(&mut connection, session_id, 2).await?;

    assert_eq!(settings.precedence.per_call, requested);
    assert_eq!(
        settings.effective.reasoning_level,
        Some(ReasoningLevel::Low)
    );
    assert_eq!(settings.reasoning_source, Some(ModelSettingSource::PerCall));
    assert_eq!(
        settings.validated_for_selection_id,
        Some(primary_direct_selection_id())
    );

    drop(connection);
    runtime.stop().await
}

/// a stop racing an active tool round never wedges the session. Against the parked approval wait
/// the stop is refused fail-closed with the wait intact; after the pending request is denied
/// through its canonical decision command, the stop cancels the turn with the denial recorded, and
/// the session accepts ordinary later input whose transcript replays cleanly.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn stop_against_a_tool_round_stays_fail_closed_then_deny_and_stop_release()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xD1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("stop during the approval wait")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::InterruptUnavailableWhileAwaitingApproval {
            session_id,
            active_turn_id: parked_turn_id,
        }
    );

    let parked = read_transcript_messages(&mut connection, 4, session_id).await?;
    assert_eq!(
        turn_state_of(&parked, parked_turn_id),
        TurnState::ActiveAwaitingToolApproval {
            tool_request_id: pending_request_id,
        }
    );
    assert_eq!(
        tool_use_entry_names(&parked, pending_request_id),
        vec![String::from("confirmed")],
        "the pending request's identity and tool name are client-visible"
    );

    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Deny {
                    reason: String::from("stop the tool round"),
                },
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (
            pending_request_id,
            ToolDecision::Deny {
                reason: String::from("stop the tool round"),
            }
        )
    );

    connection
        .request_version(
            ProtocolVersion::One,
            6,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("continue after the denied round")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            7,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("ordinary later work")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let later_turn_id = accepted_successor_turn(&mut connection, session_id, 3).await?;

    let released = read_transcript_messages(&mut connection, 8, session_id).await?;
    assert!(matches!(
        turn_state_of(&released, parked_turn_id),
        TurnState::Cancelled { .. }
    ));
    assert_eq!(tool_denied_entry_count(&released, pending_request_id), 1);
    assert_eq!(cancellation_marker_count(&released, parked_turn_id), 1);
    assert!(matches!(
        turn_state_of(&released, successor_turn_id),
        TurnState::Queued { .. }
    ));
    assert!(matches!(
        turn_state_of(&released, later_turn_id),
        TurnState::Queued { .. }
    ));

    drop(connection);
    runtime.stop().await
}
