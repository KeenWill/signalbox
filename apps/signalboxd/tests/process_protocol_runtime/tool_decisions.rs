//! Tool decisions coverage.

use super::*;

/// Records a failed exec, restarts with its continuation prepared, and parks
/// the same command in the next round with the supplied approval posture.
pub(super) async fn park_after_failed_exec(
    runtime: &mut RunningRuntime,
    session_id: CanonicalUuid,
    approval: InitialToolApproval,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    use signalbox_domain::{
        DecideToolRequest, ToolApprovalDecision, ToolAttemptId, ToolAttemptObservation,
        ToolEffectClass, ToolExecutionError, ToolExecutionErrorKind, TurnAttemptId,
    };
    let session = SessionId::from_uuid(session_id.into_uuid());
    let (calls, authorized, producing_call) =
        authorize_issued_model_call(&runtime.pool, session_id).await?;
    let first = ToolRequestId::from_uuid(Uuid::now_v7());
    let second = ToolRequestId::from_uuid(Uuid::now_v7());
    let mut authorized = authorized;
    for (request, posture) in [(first, InitialToolApproval::Confirm), (second, approval)] {
        let response = ToolUsingAssistantResponse::try_from_parts(vec![
            AssistantResponsePart::ToolCall(ToolCallProposal::new(
                ToolName::try_new("unsandboxed_exec".to_owned()).expect("exec tool name"),
                NormalizedToolArguments::try_from_provider_text(
                    r#"{"program":"npm","arguments":["install","--package-lock-only","--ignore-scripts"],"working_directory":"clients/web"}"#.to_owned(),
                ).expect("exec arguments"),
            )),
        ]).expect("exec response");
        calls
            .apply_terminal_observation(
                session,
                authorized
                    .observation_correlation()
                    .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
                        response,
                        retained_input_tokens: None,
                        retained_output_tokens: None,
                    }),
                ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                    vec![ToolResponsePartIdentity::tool_call(
                        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                        request,
                        posture,
                    )],
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                    None,
                )),
                |_| panic!("no pending steering"),
            )
            .await?;
        if request == second {
            break;
        }
        let tools = calls.tool_loop_repository();
        tools
            .decide(
                DecideToolRequest::try_new(
                    DurableCommandId::from_uuid(Uuid::now_v7()),
                    first,
                    ToolApprovalDecision::Approve,
                )
                .expect("decision command"),
                || TurnAttemptId::from_uuid(Uuid::now_v7()),
            )
            .await?;
        let turn: Uuid =
            sqlx::query_scalar("SELECT turn_id FROM tool_request WHERE request_id = $1")
                .bind(first.into_uuid())
                .fetch_one(&runtime.pool)
                .await?;
        let turn = TurnId::from_uuid(turn);
        let attempt = ToolAttemptId::from_uuid(Uuid::now_v7());
        tools
            .prepare_next_attempt(session, turn, attempt, ToolEffectClass::ExternalEffect)
            .await?;
        let authority = tools.authorize_attempt(session, turn, attempt).await?;
        tools
            .commit_observation(authority.executor_fence().bind(
                ToolAttemptObservation::KnownFailed {
                    error: ToolExecutionError::new(ToolExecutionErrorKind::ExecutionFailed, None),
                },
            ))
            .await?;
        let next_call = ModelCallId::from_uuid(Uuid::now_v7());
        tools
            .prepare_continuation(
                session,
                turn,
                producing_call,
                signalbox_application::ToolContinuationIdentities::new(
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                    next_call,
                    FailedModelCallTurnIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                        ContextFrontierId::from_uuid(Uuid::now_v7()),
                    ),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                |_| panic!("no pending steering"),
            )
            .await?;
        assert_eq!(runtime.restart().await?, 0);
        let AuthorizeModelCallOutcome::Authorized(next) =
            calls.authorize_send(session, next_call).await?
        else {
            panic!("continuation call must authorize");
        };
        authorized = next;
    }
    Ok(CanonicalUuid::from_uuid(second.into_uuid()))
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn approve_exec_after_failed_predecessor() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(
        &mut connection,
        session_id,
        "retry the exec command".to_owned(),
    )
    .await?;
    drop(connection);
    let tool_request_id =
        park_after_failed_exec(&mut runtime, session_id, InitialToolApproval::Human).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request(
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (tool_request_id, ToolDecision::Approve {})
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn delegated_exec_without_judge_returns_a_replayable_rejection() -> Result<(), Box<dyn Error>>
{
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(
        &mut connection,
        session_id,
        "retry the exec command".to_owned(),
    )
    .await?;
    drop(connection);
    let tool_request_id =
        park_after_failed_exec(&mut runtime, session_id, InitialToolApproval::Delegated).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let decision = ClientRequest::DecideToolRequest {
        command_id: command()?,
        session_id,
        tool_request_id,
        decision: ToolDecision::Approve {},
    };
    connection.request(3, decision.clone()).await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestAwaitingApprovalJudge { tool_request_id }
    );
    let (judge, prepared) = prepare_exec_judge(&runtime, session_id).await?;
    let denial = ClientRequest::DecideToolRequest {
        command_id: command()?,
        session_id,
        tool_request_id,
        decision: ToolDecision::Deny {
            reason: "stop the retry".to_owned(),
        },
    };
    connection.request(4, denial).await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestAwaitingApprovalJudge { tool_request_id }
    );
    assert!(matches!(
        judge.authorize(&prepared).await?,
        signalbox_persistence::approval_judge::AuthorizeApprovalJudgeOutcome::Authorized(_)
    ));
    connection
        .request(
            5,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestAwaitingApprovalJudge { tool_request_id }
    );
    judge
        .fail(
            &prepared,
            signalbox_persistence::approval_judge::FailedApprovalJudgeDisposition::KnownFailed,
            signalbox_domain::ProviderReportedTokenUsage::unreported(),
        )
        .await?;
    connection.request(6, decision).await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestAwaitingApprovalJudge { tool_request_id }
    );
    connection
        .request(
            7,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (tool_request_id, ToolDecision::Approve {})
    );
    drop(connection);
    runtime.stop().await
}

async fn prepare_exec_judge(
    runtime: &RunningRuntime,
    session_id: CanonicalUuid,
) -> Result<
    (
        signalbox_persistence::approval_judge::PostgresApprovalJudgeRepository,
        Box<signalbox_persistence::approval_judge::PreparedApprovalJudge>,
    ),
    Box<dyn Error>,
> {
    use signalbox_persistence::approval_judge::PrepareApprovalJudgeOutcome;
    let session = SessionId::from_uuid(session_id.into_uuid());
    let turn: Uuid = sqlx::query_scalar(
        "SELECT turn_id FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'active'",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    let calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        support::parse_model_configuration(MODEL_CONFIGURATION)?.target_catalog(),
        ModelCallCredentialReference::new("approval-fixture"),
    );
    let judge = calls.approval_judge_repository();
    let PrepareApprovalJudgeOutcome::Ready(prepared) = judge
        .prepare(
            session,
            TurnId::from_uuid(turn),
            ModelCallId::from_uuid(Uuid::now_v7()),
            None,
        )
        .await?
    else {
        panic!("delegated request must prepare its judge")
    };
    Ok((judge, prepared))
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn deny_exec_after_failed_predecessor_and_judge() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, turn_id) = submit_first_input(
        &mut connection,
        session_id,
        "retry the exec command".to_owned(),
    )
    .await?;
    drop(connection);
    let tool_request_id =
        park_after_failed_exec(&mut runtime, session_id, InitialToolApproval::Delegated).await?;
    let (judge, prepared) = prepare_exec_judge(&runtime, session_id).await?;
    judge
        .fail(
            &prepared,
            signalbox_persistence::approval_judge::FailedApprovalJudgeDisposition::KnownFailed,
            signalbox_domain::ProviderReportedTokenUsage::unreported(),
        )
        .await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let decision = ToolDecision::Deny {
        reason: "stop the retry".to_owned(),
    };
    connection
        .request(
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id,
                decision: decision.clone(),
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (tool_request_id, decision)
    );
    connection
        .request(
            4,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: turn_id,
                content: UserInputContent::text("continue after stop".to_owned()),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    accepted_successor_turn(&mut connection, session_id, 2).await?;
    let messages = read_transcript_messages(&mut connection, 5, session_id).await?;
    assert!(matches!(
        turn_state_of(&messages, turn_id),
        TurnState::Cancelled { .. }
    ));
    drop(connection);
    runtime.stop().await
}

/// a decision naming a later request while an earlier one is undecided records the exact
/// proposal-order rejection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_refuses_a_later_request_first() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let first_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    let second_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE2));
    park_turn_on_tool_approval(
        &runtime.pool,
        session_id,
        &[first_request_id, second_request_id],
    )
    .await?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: second_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestNotEarliestUndecided {
            tool_request_id: second_request_id,
            earliest_tool_request_id: first_request_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// an unknown logical request records the exact absent-request rejection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_reports_an_unknown_request() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let unknown_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE3));
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: unknown_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestNotFound {
            tool_request_id: unknown_request_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// the session-correlation precondition refuses a decision whose named session does not own the
/// named request, before any durable command is recorded.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_refuses_a_misrouted_session_without_recording()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let mut foreign = Connection::connect(runtime.socket()).await?;
    let foreign_session_id = create_alias_session(&mut foreign).await?;
    let misrouted_command = command()?;
    foreign
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::DecideToolRequest {
                command_id: misrouted_command,
                session_id: foreign_session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut foreign).await?.message()),
        RejectionDetail::ToolRequestNotInSession {
            session_id: foreign_session_id,
            tool_request_id: pending_request_id,
        }
    );
    let misrouted_claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(misrouted_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(
        misrouted_claim_count, 0,
        "the session-correlation refusal must record no durable command"
    );

    drop(foreign);
    drop(connection);
    runtime.stop().await
}

/// a denial reason outside the domain contract is refused as an invalid request before any durable
/// command is recorded.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_refuses_an_unsafe_denial_reason_before_recording()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let unsafe_reason_command = command()?;
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::DecideToolRequest {
                command_id: unsafe_reason_command,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Deny {
                    reason: String::from(" padded "),
                },
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
    let unsafe_claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(unsafe_reason_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(unsafe_claim_count, 0);

    drop(connection);
    runtime.stop().await
}

/// one durable decision identity has one recorded meaning — an equal
/// replay returns the exact recorded receipt, a different payload under the
/// same identity is conflicting reuse, and reusing an identity claimed by
/// another command kind is conflicting reuse too. The steps share one recorded
/// command, so they are asserted against the same durable state in one
/// execution.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_replays_equally_and_refuses_conflicting_reuse()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let denial_command = command()?;
    let denial = ClientRequest::DecideToolRequest {
        command_id: denial_command,
        session_id,
        tool_request_id: pending_request_id,
        decision: ToolDecision::Deny {
            reason: String::from("writes outside the workspace"),
        },
    };
    connection
        .request_version(ProtocolVersion::One, 3, denial.clone())
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (
            pending_request_id,
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            }
        )
    );

    connection
        .request_version(ProtocolVersion::One, 4, denial)
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (
            pending_request_id,
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            }
        ),
        "an equal decision replay returns its exact recorded receipt"
    );

    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::DecideToolRequest {
                command_id: denial_command,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::ConflictingReuse,
            ..
        }
    ));

    let submit_command = command()?;
    connection
        .request_version(
            ProtocolVersion::One,
            6,
            ClientRequest::SubmitInput {
                command_id: submit_command,
                session_id,
                content: UserInputContent::text(String::from("claims a submit identity")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    assert!(matches!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ActiveTurnPresent { .. }
    ));
    connection
        .request_version(
            ProtocolVersion::One,
            7,
            ClientRequest::DecideToolRequest {
                command_id: submit_command,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert!(
        matches!(
            response_within(&mut connection).await?.message(),
            ServerMessage::Error {
                code: ErrorCode::ConflictingReuse,
                ..
            }
        ),
        "an identity claimed by another command kind is conflicting reuse"
    );

    drop(connection);
    runtime.stop().await
}

/// the final approval opens the executing phase.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_final_approval_opens_the_executing_phase() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, decided_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (pending_request_id, ToolDecision::Approve {})
    );

    let decided = read_transcript_messages(&mut connection, 4, session_id).await?;
    assert!(
        matches!(
            turn_state_of(&decided, decided_turn_id),
            TurnState::ActiveRunning { .. }
        ),
        "the final approval opens the executing phase"
    );

    drop(connection);
    runtime.stop().await
}

/// A decision in flight when the daemon drains is either acknowledged and
/// durable or unacknowledged and unclaimed; after the restart the same command
/// replays to one applied decision with one `delivered` receipt.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_survives_a_drain_and_restart() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let decision_command = command()?;
    let decision = ClientRequest::DecideToolRequest {
        command_id: decision_command,
        session_id,
        tool_request_id: pending_request_id,
        decision: ToolDecision::Approve {},
    };
    connection
        .request_version(ProtocolVersion::One, 3, decision.clone())
        .await?;
    runtime.shutdown.send_replace(true);
    // Whether the drain let this reply through or closed the socket first is
    // the race under test; either way the replay below settles the decision.
    let _acknowledgement = timeout(Duration::from_secs(5), connection.response()).await;
    drop(connection);
    runtime.restart().await?;

    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request_version(ProtocolVersion::One, 4, decision)
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (pending_request_id, ToolDecision::Approve {}),
        "the drained decision replays as one applied decision"
    );
    let settled: (i64, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM tool_approval_decision WHERE request_id = $1),
                receipt.outcome_kind
           FROM injection_settled_outbox_event AS receipt
          WHERE receipt.command_id = $2",
    )
    .bind(pending_request_id.into_uuid())
    .bind(decision_command.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(settled, (1, String::from("delivered")));

    drop(connection);
    runtime.stop().await
}

/// a request that already has a terminal resolution records the exact already-resolved rejection
/// for a later distinct decision.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_refuses_an_already_resolved_request() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (pending_request_id, ToolDecision::Approve {})
    );

    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestAlreadyResolved {
            tool_request_id: pending_request_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn placement_loss_request_refuses_approval_before_result_projection()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(
        &mut connection,
        session_id,
        "approval closure fixture".to_owned(),
    )
    .await?;
    let closed = CanonicalUuid::from_uuid(Uuid::now_v7());
    let pending = CanonicalUuid::from_uuid(Uuid::now_v7());
    park_turn_on_tool_approval(&runtime.pool, session_id, &[closed, pending]).await?;
    let mut transaction = runtime.pool.begin().await?;
    // Seed the retained request resolution; loss transaction coverage owns its runner evidence.
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER tool_request_resolution_guard")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE tool_request SET resolution_kind = 'closed_inadmissible', inadmissible_reason = 'placement_lost' WHERE request_id = $1")
        .bind(closed.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("UPDATE turn_lifecycle SET approval_tool_request_id = $1 WHERE session_id = $2 AND state_kind = 'active'")
        .bind(pending.into_uuid()).bind(session_id.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER tool_request_resolution_guard")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    connection
        .request(
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: closed,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestAlreadyResolved {
            tool_request_id: closed
        }
    );
    let entries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM semantic_transcript_entry WHERE tool_result_request_id = $1",
    )
    .bind(closed.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(entries, 0);
    drop(connection);
    runtime.stop().await
}
