//! Input admission coverage.

use super::*;

pub(crate) const OVERSIZED_SUBMITTED_INPUT_BYTES: usize = MAX_SUBMITTED_INPUT_BYTES + 1;
pub(crate) async fn read_goal_messages(
    connection: &mut Connection,
    request_id: u64,
    session_id: CanonicalUuid,
) -> Result<Vec<ServerMessage>, Box<dyn Error>> {
    connection
        .request(request_id, ClientRequest::ReadGoal { session_id })
        .await?;
    let mut messages = Vec::new();
    loop {
        let message = response_within(connection).await?.message().clone();
        let ended = matches!(message, ServerMessage::GoalHistoryEnd { .. });
        messages.push(message);
        if ended {
            return Ok(messages);
        }
    }
}

/// One commission request atomically creates a template session under a
/// recorded authority fence with its goal and first input; the same command
/// identity replays to the committed session, and the same identity naming a
/// different fence is a conflicting reuse.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn commission_session_records_its_fence_goal_and_first_input() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let commission_command = command()?;
    let statement = String::from("Address the review findings on pull request 41.");
    let fence = CommissionedSessionFence::PullRequest {
        repository: String::from("sample-user/sample-repository"),
        pull_request: CanonicalU64::new(41),
        head_sha: String::from("1111111111111111111111111111111111111111"),
        head_repository: String::from("sample-user/sample-repository"),
        head_branch: String::from("agent/sample-feature"),
        base_branch: String::from("main"),
    };
    let request = ClientRequest::CommissionSession {
        command_id: commission_command,
        template_name: String::from("merge-forward"),
        fence: fence.clone(),
        statement: statement.clone(),
        content: InputContent::new(String::from("Respond to the open review threads.")),
    };

    connection.request(2, request.clone()).await?;
    let commissioned = response_within(&mut connection).await?.message().clone();
    let ServerMessage::SessionCommissioned {
        session_id,
        dispatch_id,
    } = commissioned
    else {
        panic!("unexpected commission response: {commissioned:?}");
    };

    connection.request(3, request.clone()).await?;
    let replayed = response_within(&mut connection).await?.message().clone();
    assert_eq!(
        replayed,
        ServerMessage::SessionCommissioned {
            session_id,
            dispatch_id,
        }
    );

    connection
        .request(
            4,
            ClientRequest::CommissionSession {
                command_id: command()?,
                template_name: String::from("merge-forward"),
                fence: fence.clone(),
                statement: statement.clone(),
                content: InputContent::new(String::from("Respond to the open review threads.")),
            },
        )
        .await?;
    let busy = response_within(&mut connection).await?.message().clone();
    assert_eq!(protocol_error_code(&busy), ErrorCode::Rejected);
    assert_eq!(
        protocol_error_detail(&busy),
        Some(RejectionDetail::CommissionTargetBusy { session_id })
    );

    connection
        .request(
            5,
            ClientRequest::CommissionSession {
                command_id: commission_command,
                template_name: String::from("merge-forward"),
                fence: CommissionedSessionFence::Branch {
                    repository: String::from("sample-user/sample-repository"),
                    branch: String::from("main"),
                },
                statement: statement.clone(),
                content: InputContent::new(String::from("Respond to the open review threads.")),
            },
        )
        .await?;
    let conflicting = response_within(&mut connection).await?.message().clone();
    let ServerMessage::Error { code, .. } = conflicting else {
        panic!("a conflicting commission reuse must be refused: {conflicting:?}");
    };
    assert_eq!(code, ErrorCode::ConflictingReuse);

    let history = read_goal_messages(&mut connection, 6, session_id).await?;
    assert_eq!(
        history.first(),
        Some(&ServerMessage::GoalHistoryStart {
            session_id,
            current_generation: CanonicalU64::new(1),
            current_statement: statement.clone(),
        })
    );

    // Template-configuration drift: restart over the same database with every
    // template removed. The committed commission stays discoverable through
    // the exact retry, because replay is resolved from the durable record
    // before the live template catalog is consulted.
    runtime.restart_without_templates().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection.request(7, request).await?;
    let drift_replayed = response_within(&mut connection).await?.message().clone();
    assert_eq!(
        drift_replayed,
        ServerMessage::SessionCommissioned {
            session_id,
            dispatch_id,
        }
    );

    // A fresh commission naming the removed template is still refused: only
    // replay of committed work survives configuration drift.
    connection
        .request(
            8,
            ClientRequest::CommissionSession {
                command_id: command()?,
                template_name: String::from("merge-forward"),
                fence,
                statement,
                content: InputContent::new(String::from("Respond to the open review threads.")),
            },
        )
        .await?;
    let refused = response_within(&mut connection).await?.message().clone();
    let ServerMessage::Error { code, .. } = refused else {
        panic!("a fresh commission under a removed template must refuse: {refused:?}");
    };
    assert_eq!(code, ErrorCode::InvalidRequest);
    Ok(())
}

/// process goal commands preserve immutable supersession lineage and
/// show returns the complete ordered event stream with its current projection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s_goal_process_protocol_supersession_history_round_trips() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let attach_command = command()?;
    let supersede_command = command()?;
    let stop_command = command()?;
    let first_statement = String::from("finish the commissioned task");
    let replacement_statement = String::from("finish the clarified task");

    connection
        .request(
            2,
            ClientRequest::AttachGoal {
                command_id: attach_command,
                session_id,
                statement: first_statement.clone(),
            },
        )
        .await?;
    let attached = response_within(&mut connection).await?.message().clone();
    connection
        .request(
            3,
            ClientRequest::SupersedeGoal {
                command_id: supersede_command,
                session_id,
                statement: replacement_statement.clone(),
            },
        )
        .await?;
    let superseded = response_within(&mut connection).await?.message().clone();
    connection
        .request(
            4,
            ClientRequest::StopGoal {
                command_id: stop_command,
                session_id,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        )
        .await?;
    let stopped = response_within(&mut connection).await?.message().clone();
    let history = read_goal_messages(&mut connection, 5, session_id).await?;

    assert_eq!(
        attached,
        ServerMessage::GoalTransitionApplied {
            termination: None,
            session_id,
            event_ordinal: CanonicalU64::new(1),
            generation: CanonicalU64::new(1),
        }
    );
    assert_eq!(
        superseded,
        ServerMessage::GoalTransitionApplied {
            termination: None,
            session_id,
            event_ordinal: CanonicalU64::new(2),
            generation: CanonicalU64::new(1),
        }
    );
    assert_eq!(
        stopped,
        ServerMessage::GoalTransitionApplied {
            termination: Some(signalbox_process_protocol::TerminationReceipt {
                descendant_scope: DescendantTerminationScope::ParentAlone,
                descendant_count: CanonicalU64::new(0),
            }),
            session_id,
            event_ordinal: CanonicalU64::new(3),
            generation: CanonicalU64::new(2),
        }
    );
    assert_eq!(
        history,
        vec![
            ServerMessage::GoalHistoryStart {
                session_id,
                current_generation: CanonicalU64::new(2),
                current_statement: replacement_statement.clone(),
            },
            ServerMessage::GoalHistoryState {
                current_state: GoalLifecycleState::UserStopped {},
            },
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(1),
                generation: CanonicalU64::new(1),
                event: GoalHistoryEvent::Commissioned {
                    statement: first_statement,
                    command_id: attach_command,
                },
            },
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(2),
                generation: CanonicalU64::new(1),
                event: GoalHistoryEvent::Superseded {
                    replacement_statement,
                    command_id: supersede_command,
                },
            },
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(3),
                generation: CanonicalU64::new(2),
                event: GoalHistoryEvent::UserStopped {
                    command_id: stop_command,
                    settling_turn_id: None,
                    abandoned_actions: Some(CanonicalU64::new(0)),
                },
            },
            ServerMessage::GoalHistoryEnd {
                event_count: CanonicalU64::new(3),
            },
        ]
    );

    drop(connection);
    runtime.stop().await
}

pub(crate) async fn complete_active_text_turn(
    pool: &PgPool,
    session: SessionId,
    targets: ModelTargetCatalog,
) -> Result<(), Box<dyn Error>> {
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("process-runtime-fixture"),
    );
    let mut service = ModelCallExecutionService::new(
        UuidV7ModelCallExecutionIdGenerator,
        repository.clone(),
        repository.clone(),
        repository.clone(),
        repository,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new(String::from("fixture response"))
                        .expect("fixture assistant content is valid"),
                ],
            },
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );
    assert!(matches!(
        service.execute(session).await?,
        ModelCallExecutionOutcome::Checkpointed(_)
    ));
    assert!(matches!(
        service.execute(session).await?,
        ModelCallExecutionOutcome::ObservationCommitted(outcome)
            if matches!(*outcome, ModelCallTerminalOutcome::Completed(_))
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_rejects_oversized_submitted_input() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;

    let frame = serde_json::json!({
        "version": 1,
        "request_id": "2",
        "request": {
            "type": "submit_input",
            "command_id": command()?,
            "session_id": session_id,
            "content": [{
                "type": "text",
                "text": "x".repeat(OVERSIZED_SUBMITTED_INPUT_BYTES),
            }],
            "expected_defaults_version": "1",
            "model_settings": {
                "reasoning_level": { "kind": "inherit" },
                "fast_mode": { "kind": "inherit" },
                "service_tier": { "kind": "inherit" },
            },
        },
    });
    connection.raw_request(&format!("{frame}\n")).await?;

    let response = response_within(&mut connection).await?;
    assert!(matches!(
        response.message(),
        ServerMessage::Error {
            code: ErrorCode::MalformedFrame,
            ..
        }
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_admits_exact_limit_submitted_input() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;

    let _submitted = submit_first_input(
        &mut connection,
        session_id,
        "x".repeat(MAX_SUBMITTED_INPUT_BYTES),
    )
    .await?;

    drop(connection);
    runtime.stop().await
}

pub(crate) async fn submit_queued_input(
    connection: &mut Connection,
    request_id: u64,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    acceptance_position: u64,
    content: &str,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    connection
        .request_version(
            ProtocolVersion::One,
            request_id,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(content.to_owned()),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::Queue {
                    expected_active_turn_id,
                }),
            },
        )
        .await?;
    accepted_successor_turn(connection, session_id, acceptance_position).await
}

pub(crate) async fn activate_expected_turn(
    pool: &PgPool,
    session: SessionId,
    expected_turn: CanonicalUuid,
) -> Result<(), Box<dyn Error>> {
    let mut service = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    match service.execute(session).await? {
        StartEligibleTurnOutcome::Activated(activated)
            if activated.turn().into_uuid() == expected_turn.into_uuid() =>
        {
            let recorded =
                signalboxd::WorkspaceInstructionRuntime::new(pool.clone(), None, Vec::new())
                    .prepare(session, activated.turn())
                    .await?;
            if !recorded {
                return Err(
                    io::Error::other("the fixture instruction manifest must record").into(),
                );
            }
            Ok(())
        }
        StartEligibleTurnOutcome::Activated(activated) => Err(io::Error::other(format!(
            "activated turn {} instead of expected {expected_turn}",
            activated.turn().into_uuid()
        ))
        .into()),
        StartEligibleTurnOutcome::NoEligibleTurn => {
            Err(io::Error::other("the expected queued turn was not eligible").into())
        }
    }
}

/// steering against an idle session is a durable-submit refusal with the exact expected turn, never
/// an internal daemon error.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn steering_without_an_active_turn_is_a_typed_rejection() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let expected_active_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(0x1301));

    connection
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("steer no turn")),
                expected_defaults_version: None,
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::Steer {
                    expected_active_turn_id,
                }),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::NoActiveTurn {
            session_id,
            expected_active_turn_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// two after-current-turn inputs stay queued until the occupied slot terminalizes, then activate in
/// immutable acceptance order.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn queued_inputs_deliver_in_acceptance_order_after_the_active_turn()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, active_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("active request")).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;

    let first_queued_turn = submit_queued_input(
        &mut connection,
        3,
        session_id,
        active_turn_id,
        2,
        "first queued request",
    )
    .await?;
    let second_queued_turn = submit_queued_input(
        &mut connection,
        4,
        session_id,
        active_turn_id,
        3,
        "second queued request",
    )
    .await?;
    assert_ne!(first_queued_turn, second_queued_turn);

    let targets = support::parse_model_configuration(MODEL_CONFIGURATION)?.target_catalog();
    complete_active_text_turn(&runtime.pool, session, targets.clone()).await?;
    activate_expected_turn(&runtime.pool, session, first_queued_turn).await?;
    complete_active_text_turn(&runtime.pool, session, targets).await?;
    activate_expected_turn(&runtime.pool, session, second_queued_turn).await?;

    drop(connection);
    runtime.stop().await
}

/// an acknowledged after-current-turn input remains durable across an actual process stop, startup
/// scan, and listener restart, then activates as the exact queued turn after the abandoned active
/// turn is recovered.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn queued_input_survives_process_restart_and_startup_scan() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, active_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("active request")).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;
    let queued_turn_id = submit_queued_input(
        &mut connection,
        3,
        session_id,
        active_turn_id,
        2,
        "durable queued request",
    )
    .await?;
    drop(connection);

    assert_eq!(runtime.restart().await?, 1);
    activate_expected_turn(&runtime.pool, session, queued_turn_id).await?;

    runtime.stop().await
}

/// the provider-reported preflight scores the queued turn's own input. Reported usage that fits on
/// its own exhausts the reserved headroom once the waiting input is counted, and the daemon
/// compacts that queued turn before activating it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reported_usage_preflight_counts_queued_input_framing() -> Result<(), Box<dyn Error>> {
    let configuration_text = reported_usage_preflight_configuration_text();
    let mut runtime = RunningRuntime::start_with_model_configuration(&configuration_text).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, first_turn) = submit_first_input(
        &mut connection,
        session_id,
        String::from("queued input preflight historical request"),
    )
    .await?;
    // `reported_usage_preflight_configuration_text` declares a 4096-token
    // window with a 16-token output reservation, so this reported input leaves
    // 80 tokens of headroom on its own.
    let fitting_usage = TokenUsage {
        input_tokens: Some(4000),
        output_tokens: Some(0),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    let first_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "queued input preflight historical reply",
        fitting_usage,
    ));
    let first_probe = execute_streamed_turn_until_with_configuration(
        &mut runtime,
        first_runtime,
        reported_usage_preflight_configuration()?,
        session_id,
        first_turn,
        TurnSettle::Terminal,
    )
    .await?;
    assert_eq!(first_probe.received_operations().len(), 1);

    // The 63 content bytes fit the remaining 80 tokens; the actual OpenAI
    // message envelope makes this uncommitted input exceed that headroom.
    let queued_input =
        String::from("queued input whose bytes fit but whose rendered framing does not");
    connection
        .request_version(
            ProtocolVersion::One,
            40,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(queued_input),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut connection, session_id, 2).await?;
    let configuration = reported_usage_preflight_configuration()?;
    let runtime_models = configuration.runtime_model_catalog();
    let summary_text = String::from("queued input preflight summary");
    let summary_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        &summary_text,
        TokenUsage {
            input_tokens: Some(4000),
            output_tokens: Some(20),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        },
    ));
    let summary_probe = summary_runtime.clone();
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            summary_runtime,
            runtime_models.clone(),
        ));
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("queued-input-preflight-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog());
    let compaction = ReportedUsageCompaction::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        repository,
        NoToolCatalog,
        runtime_models,
        configuration,
        compaction_model,
    );

    // No occupancy-recovery window surrounds this fixture's guard call, so
    // preparation has no window to name its compaction to. That is the case the
    // window's own contract states: a window that prepared nothing owes no
    // recovery. What this fixture exercises is the queued-turn headroom
    // arithmetic, not window-named recovery.
    compaction
        .compact_if_needed(SessionId::from_uuid(session_id.into_uuid()), None)
        .await?;

    assert_eq!(summary_probe.received_operations().len(), 1);
    let compaction_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM context_compaction
          WHERE session_id = $1",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(compaction_count, 1);
    let summary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND payload_kind = 'context_summary'
            AND context_summary_value = $2",
    )
    .bind(session_id.into_uuid())
    .bind(&summary_text)
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(summary_count, 1);
    let lifecycle: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(lifecycle, (String::from("queued"), None));

    drop(connection);
    runtime.stop().await
}

/// A cascading stop replays its stored zero-child choice after another goal command.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn cascading_goal_receipt_replays_recorded_scope_and_count() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    connection
        .request(
            2,
            ClientRequest::AttachGoal {
                command_id: command()?,
                session_id,
                statement: "complete the fixture task".into(),
            },
        )
        .await?;
    let attached = response_within(&mut connection).await?.message().clone();
    assert!(
        matches!(attached, ServerMessage::GoalTransitionApplied { .. }),
        "{attached:?}"
    );
    let stop_command = command()?;
    let stop = ClientRequest::StopGoal {
        command_id: stop_command,
        session_id,
        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
    };
    connection.request(3, stop.clone()).await?;
    let recorded = response_within(&mut connection).await?.message().clone();
    assert!(
        matches!(&recorded, ServerMessage::GoalTransitionApplied {
        termination: Some(signalbox_process_protocol::TerminationReceipt {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            descendant_count,
        }), ..
    } if descendant_count.value() == 0),
        "{recorded:?}"
    );
    connection
        .request(
            4,
            ClientRequest::ResumeGoal {
                command_id: command()?,
                session_id,
                guidance: None,
            },
        )
        .await?;
    response_within(&mut connection).await?;
    connection.request(5, stop).await?;
    assert_eq!(response_within(&mut connection).await?.message(), &recorded);
    Ok(())
}
