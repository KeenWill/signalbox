//! Tool-result compaction preserves active turns and their commissioned goals.

use super::*;
use signalbox_application::{EligibilityWorkSource, ToolCatalog};
use signalbox_domain::{
    CreateSession, ModuleDispatch, ProviderReportedTokenUsage, RepoWatchDispatchId,
    SessionCreationCause, SessionCreationProvenance, SessionOwnership, StartGate,
    TranscriptAncestry,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository, model_execution::ToolContinuationUsageLimit,
};

#[derive(Clone, Copy)]
enum ContinuationSession {
    Interactive,
    RepositoryWatch,
    /// Created held with an external finish gate, commissioned before release.
    CommissionedRepositoryWatch,
    /// Commissioned with the fixture alias, which is removed before execution recovery.
    CommissionedRepositoryWatchViaRemovedAlias,
}

async fn queued_continuation_session(
    runtime: &RunningRuntime,
    kind: ContinuationSession,
) -> Result<(SessionId, TurnId), Box<dyn Error>> {
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let session = SessionId::from_uuid(Uuid::now_v7());
    let provenance = if !matches!(kind, ContinuationSession::Interactive) {
        SessionCreationProvenance::module_dispatched(ModuleDispatch::RepositoryWatch {
            dispatch: RepoWatchDispatchId::from_uuid(Uuid::now_v7()),
        })
    } else {
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None)
    };
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::now_v7()),
        provenance,
        SessionConfigurationDefaults::new(match kind {
            ContinuationSession::CommissionedRepositoryWatchViaRemovedAlias => {
                ModelSelectionRequest::Alias(signalbox_domain::ModelAlias::from_uuid(
                    Uuid::from_u128(2),
                ))
            }
            _ => ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(1))),
        }),
    )
    .with_lifecycle(
        if matches!(
            kind,
            ContinuationSession::CommissionedRepositoryWatch
                | ContinuationSession::CommissionedRepositoryWatchViaRemovedAlias
        ) {
            StartGate::Held
        } else {
            StartGate::Open
        },
        SessionOwnership::Owned,
        if matches!(
            kind,
            ContinuationSession::CommissionedRepositoryWatch
                | ContinuationSession::CommissionedRepositoryWatchViaRemovedAlias
        ) {
            Some(signalbox_domain::FinishCondition::ExternalGate)
        } else {
            None
        },
    )
    .prepare(session)
    .expect("the fixture creates a pathless owned session");
    CreateSessionRepository::new(runtime.pool.clone(), configuration.session_credential_pin())
        .handle(creation)
        .await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let wire_session = CanonicalUuid::from_uuid(session.into_uuid());
    let turn = match kind {
        ContinuationSession::CommissionedRepositoryWatch
        | ContinuationSession::CommissionedRepositoryWatchViaRemovedAlias => {
            use signalbox_domain::{
                AcceptedInputId, CommandPrincipal, GoalStatement, GoalUserAction, GoalUserCommand,
                SessionLifecycleCommand, SessionLifecycleOperation,
            };
            use signalbox_persistence::{
                goal::GoalRepository, goal_turn::GoalTurnCandidates,
                session_lifecycle_command::SessionLifecycleCommandRepository,
            };
            let candidates = GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::now_v7()),
                TurnId::from_uuid(Uuid::now_v7()),
            );
            let attached = GoalRepository::new(runtime.pool.clone())
                .handle_user_command(
                    GoalUserCommand::new(
                        DurableCommandId::from_uuid(Uuid::now_v7()),
                        session,
                        GoalUserAction::Attach(GoalStatement::try_new(
                            "Finish the repository change".to_owned(),
                        )?),
                    ),
                    Some(candidates),
                    |alias| configuration.resolve_alias(alias),
                )
                .await?;
            assert!(matches!(
                attached,
                signalbox_persistence::goal::GoalCommandHandlingOutcome::Recorded(
                    signalbox_domain::GoalCommandResult::Applied(_)
                )
            ));
            SessionLifecycleCommandRepository::new(runtime.pool.clone())
                .handle(
                    SessionLifecycleCommand::new(
                        DurableCommandId::from_uuid(Uuid::now_v7()),
                        session,
                        SessionLifecycleOperation::ReleaseStart,
                    ),
                    CommandPrincipal::Core,
                )
                .await?;
            candidates.turn()
        }
        ContinuationSession::Interactive | ContinuationSession::RepositoryWatch => {
            connection
                .request(
                    2,
                    ClientRequest::SubmitInput {
                        command_id: command()?,
                        session_id: wire_session,
                        content: UserInputContent::text(String::from(
                            "Finish the repository change",
                        )),
                        expected_defaults_version: Some(CanonicalU64::new(1)),
                        model_settings: ModelSettingsOverlay::inherit_all(),
                        delivery: None,
                    },
                )
                .await?;
            let wire_turn = accepted_successor_turn(&mut connection, wire_session, 1).await?;
            TurnId::from_uuid(wire_turn.into_uuid())
        }
    };
    Ok((session, turn))
}

async fn exhausted_continuation(
    runtime: &RunningRuntime,
    kind: ContinuationSession,
) -> Result<(SessionId, TurnId), Box<dyn Error>> {
    let (session, turn) = queued_continuation_session(runtime, kind).await?;
    let wire_session = CanonicalUuid::from_uuid(session.into_uuid());
    let mut connection = Connection::connect(runtime.socket()).await?;
    let (calls, authorized, producing_call) =
        Box::pin(authorize_issued_model_call(&runtime.pool, wire_session)).await?;
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from("fixture_tool")).expect("fixture name"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("fixture arguments"),
            ),
        )])
        .expect("one tool proposal");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation_with_usage(
            ModelCallTerminalObservation::CompletedWithTools {
                response,
                retained_input_tokens: None,
                retained_output_tokens: None,
            },
            // More than MODEL_CONFIGURATION's context window, forcing the continuation guard.
            ProviderReportedTokenUsage::unreported().with_input_tokens(Some(300_000)),
        );
    let request = ToolRequestId::from_uuid(Uuid::now_v7());
    calls
        .apply_terminal_observation(
            session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    request,
                    InitialToolApproval::Confirm,
                )],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                None,
            )),
            |_| panic!("no steering in this fixture"),
        )
        .await?;
    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id: wire_session,
                tool_request_id: CanonicalUuid::from_uuid(request.into_uuid()),
                decision: ToolDecision::Deny {
                    reason: String::from("fixture denies execution"),
                },
            },
        )
        .await?;
    let decision = response_within(&mut connection).await?;
    assert!(!matches!(decision.message(), ServerMessage::Error { .. }));
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(3)));
    let calls = calls.with_continuation_usage_limits([ToolContinuationUsageLimit::new(
        target,
        signalbox_domain::FastMode::Disabled,
        256,
        200_000,
    )]);
    let outcome = calls
        .tool_loop_repository()
        .prepare_continuation(
            session,
            turn,
            producing_call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                ModelCallId::from_uuid(Uuid::now_v7()),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            |_| panic!("no steering in this fixture"),
        )
        .await?;
    assert!(matches!(
        outcome,
        signalbox_application::PrepareToolContinuationOutcome::ContextCompactionRequired(_)
    ));
    Ok((session, turn))
}

/// Sequential read rounds leave safe boundaries between their large results.
async fn exhausted_read_heavy_continuation(
    runtime: &RunningRuntime,
) -> Result<(SessionId, TurnId), Box<dyn Error>> {
    use signalbox_domain::{
        DecideToolRequest, ToolApprovalDecision, ToolAttemptId, ToolAttemptObservation,
        ToolEffectClass, ToolResultContent, TurnAttemptId,
    };
    let (session, turn) =
        queued_continuation_session(runtime, ContinuationSession::RepositoryWatch).await?;
    let (calls, mut authorized, mut producing_call) =
        authorize_issued_model_call(&runtime.pool, CanonicalUuid::from_uuid(session.into_uuid()))
            .await?;
    // Forty results include one 269 KiB batch, beyond the compaction input window.
    let result_text = "r".repeat(16_000);
    for round in 0..40 {
        let request = ToolRequestId::from_uuid(Uuid::now_v7());
        let response =
            ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
                ToolCallProposal::new(
                    ToolName::try_new("fixture_read".to_owned()).expect("fixture read name"),
                    NormalizedToolArguments::try_from_provider_text("{}".to_owned())
                        .expect("fixture read arguments"),
                ),
            )])
            .expect("one read proposal");
        calls
            .apply_terminal_observation(
                session,
                authorized
                    .observation_correlation()
                    .bind_terminal_observation_with_usage(
                        ModelCallTerminalObservation::CompletedWithTools {
                            response,
                            retained_input_tokens: None,
                            retained_output_tokens: None,
                        },
                        ProviderReportedTokenUsage::unreported().with_input_tokens(Some(300_000)),
                    ),
                ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                    vec![ToolResponsePartIdentity::tool_call(
                        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                        request,
                        InitialToolApproval::Confirm,
                    )],
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                    None,
                )),
                |_| panic!("no steering in the read fixture"),
            )
            .await?;
        let tools = calls.tool_loop_repository();
        tools
            .decide(
                DecideToolRequest::try_new(
                    DurableCommandId::from_uuid(Uuid::now_v7()),
                    request,
                    ToolApprovalDecision::Approve,
                )
                .expect("approve fixture read"),
                || TurnAttemptId::from_uuid(Uuid::now_v7()),
            )
            .await?;
        let attempt = ToolAttemptId::from_uuid(Uuid::now_v7());
        tools
            .prepare_next_attempt(session, turn, attempt, ToolEffectClass::EffectFree)
            .await?;
        let authority = tools.authorize_attempt(session, turn, attempt).await?;
        tools
            .commit_observation(
                authority
                    .executor_fence()
                    .bind(ToolAttemptObservation::Completed {
                        result: ToolResultContent::Text(
                            signalbox_domain::ToolResultText::try_new(if round == 39 {
                                "r".repeat(269 * 1024)
                            } else {
                                result_text.clone()
                            })
                            .expect("bounded read result"),
                        ),
                    }),
            )
            .await?;
        let next_call = ModelCallId::from_uuid(Uuid::now_v7());
        let continuation_calls = if round == 39 {
            calls
                .clone()
                .with_continuation_usage_limits([ToolContinuationUsageLimit::new(
                    ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                        Uuid::from_u128(3),
                    )),
                    signalbox_domain::FastMode::Disabled,
                    256,
                    200_000,
                )])
        } else {
            calls.clone()
        };
        let outcome = continuation_calls
            .tool_loop_repository()
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
                |_| panic!("no steering in the read fixture"),
            )
            .await?;
        if round == 39 {
            assert!(matches!(
                outcome,
                signalbox_application::PrepareToolContinuationOutcome::ContextCompactionRequired(_)
            ));
            break;
        }
        let AuthorizeModelCallOutcome::Authorized(next) =
            calls.authorize_send(session, next_call).await?
        else {
            panic!("the next read round authorizes");
        };
        authorized = next;
        producing_call = next_call;
    }
    Ok((session, turn))
}

fn continuation_compaction(
    runtime: &RunningRuntime,
    summary: ScriptedModel<ModelCallId>,
) -> Result<ReportedUsageCompaction, Box<dyn Error>> {
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    continuation_compaction_with_configuration(runtime, summary, configuration)
}

fn continuation_compaction_with_configuration(
    runtime: &RunningRuntime,
    summary: ScriptedModel<ModelCallId>,
    configuration: signalboxd::HubModelConfiguration,
) -> Result<ReportedUsageCompaction, Box<dyn Error>> {
    let runtime_models = configuration.runtime_model_catalog();
    let calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("continuation-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog());
    Ok(ReportedUsageCompaction::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        calls,
        NoToolCatalog,
        runtime_models.clone(),
        configuration,
        Arc::new(RuntimeContextCompactionModel::new(summary, runtime_models)),
    )
    .with_repository_watch_continuation(
        runtime.eligibility_nudge.clone(),
        InProcessToolDispatchGate::default(),
    ))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn read_heavy_automatic_compaction_completes_the_active_turn() -> Result<(), Box<dyn Error>> {
    let configuration_text =
        MODEL_CONFIGURATION.replace("adapter = \"anthropic\"", "adapter = \"openai\"");
    let runtime = RunningRuntime::start_with_model_configuration(&configuration_text).await?;
    let (session, original) = exhausted_read_heavy_continuation(&runtime).await?;
    let summary = ScriptedModel::following((0..4).map(|_| {
        completed_script(
            "fixture-model",
            "The repository change remains unfinished.",
            TokenUsage {
                input_tokens: Some(40_000),
                output_tokens: Some(10),
                ..TokenUsage::default()
            },
        )
    }));
    let probe = summary.clone();
    let configuration = support::parse_model_configuration(&configuration_text)?;
    let (nudge, mut work_source) = InProcessEligibilityWorkSource::with_options(
        NoCompactionSweep,
        None,
        std::num::NonZeroUsize::new(1),
    );
    let compaction =
        continuation_compaction_with_configuration(&runtime, summary, configuration.clone())?
            .with_repository_watch_continuation(nudge, InProcessToolDispatchGate::default());
    let runtime_models = configuration.runtime_model_catalog();
    let ordinary = compaction::RecordingCountedScriptedModel::following(
        [completed_script(
            "fixture-model",
            "Repository task complete.",
            TokenUsage::default(),
        )],
        [100],
    );
    let ordinary_probe = ordinary.clone();
    let provider = RuntimeModelCallProvider::new(ordinary, runtime_models.clone(), None);
    let (catalog, executor) = signalboxd::goal_declaration_test_tools(runtime.pool.clone())?;
    let calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("continuation-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog())
    .with_continuation_usage_limits(
        configuration.tool_continuation_usage_limits(&catalog.definitions())?,
    );
    let execution = signalboxd::WorkspaceInstructionPreparedExecution::new(
        PostgresProviderModelExecution::new(
            calls.clone(),
            InProcessAttemptDispatchGate::default(),
            provider.clone(),
            None,
        )
        .with_tool_loop(InProcessToolDispatchGate::default(), catalog, executor),
        signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
    );
    let mut pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        calls,
        provider,
        NoToolCatalog,
        runtime_models.clone(),
        configuration,
        Arc::new(RuntimeContextCompactionModel::new(
            ScriptedModel::following([]),
            runtime_models,
        )),
        HeapAllocatedExecution(execution),
    )
    .with_reported_usage_compaction(compaction.clone())
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        runtime.pool.clone(),
        None,
        Vec::new(),
    ));
    // The persisted checkpoint remains eligible after a restart.
    let (recovered, _) = PostgresEligibilitySweep::new(runtime.pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert_eq!(recovered, vec![session]);
    for hint in recovered {
        pass.run(hint).await?;
    }
    let hint = timeout(Duration::from_secs(5), work_source.next()).await??;
    assert_eq!(hint, session);
    pass.run(hint).await?;
    let (remaining, _) = PostgresEligibilitySweep::new(runtime.pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(remaining.is_empty(), "the completed turn leaves the sweep");
    assert_eq!(ordinary_probe.prepared_operations().len(), 1);
    assert_eq!(probe.received_operations().len(), 1);
    let states: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT turn_id, terminal_disposition_kind FROM turn_lifecycle WHERE session_id = $1 ORDER BY acceptance_position",
    ).bind(session.into_uuid()).fetch_all(&runtime.pool).await?;
    assert_eq!(
        states,
        vec![(original.into_uuid(), String::from("completed"))]
    );
    runtime.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn held_goal_compaction_continues_without_an_operator() -> Result<(), Box<dyn Error>> {
    commissioned_compaction_completes_goal(ContinuationSession::CommissionedRepositoryWatch).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn a_removed_alias_does_not_block_commissioned_compaction_recovery()
-> Result<(), Box<dyn Error>> {
    commissioned_compaction_completes_goal(
        ContinuationSession::CommissionedRepositoryWatchViaRemovedAlias,
    )
    .await
}

async fn commissioned_compaction_completes_goal(
    kind: ContinuationSession,
) -> Result<(), Box<dyn Error>> {
    let runtime = Box::pin(RunningRuntime::start()).await?;
    let (session, original) = Box::pin(queued_continuation_session(&runtime, kind)).await?;
    let summary = ScriptedModel::single(completed_script(
        "fixture-model",
        "The repository change remains unfinished.",
        TokenUsage::default(),
    ));
    let probe = summary.clone();
    let configuration = match kind {
        ContinuationSession::CommissionedRepositoryWatchViaRemovedAlias => {
            // This is the alias frozen when the fixture commissions its goal.
            let retired_alias = "[[aliases]]\nalias_id = \"00000000-0000-0000-0000-000000000002\"\nselection_id = \"00000000-0000-0000-0000-000000000001\"\n";
            let configuration = support::parse_model_configuration(
                &MODEL_CONFIGURATION.replace(retired_alias, ""),
            )?;
            assert!(
                configuration
                    .resolve_alias(signalbox_domain::ModelAlias::from_uuid(Uuid::from_u128(2)))
                    .is_none()
            );
            configuration
        }
        _ => support::parse_model_configuration(MODEL_CONFIGURATION)?,
    };
    let compaction =
        continuation_compaction_with_configuration(&runtime, summary, configuration.clone())?;
    let runtime_models = configuration.runtime_model_catalog();
    let ordinary = compaction::RecordingCountedScriptedModel::following(
        [
            Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
                exchange: ExchangeFacts::default(),
                message_id: None,
                reported_model: Some(ProviderReportedModel::new("fixture-model")),
                finish: CompletionFinish::ToolUse,
                content: vec![AssistantPart::ToolCall(
                    signalbox_model_runtime::ToolCallProposal {
                        id: signalbox_model_runtime::ToolCallId::new("exhaust-context"),
                        // Missing report text makes this auto-approved call return
                        // a tool failure without declaring the goal achieved.
                        name: signalbox_model_runtime::ToolName::new("goal_declare"),
                        arguments_json: r#"{"transition":"achieved"}"#.to_owned(),
                    },
                )],
                usage: TokenUsage {
                    input_tokens: Some(300_000),
                    ..TokenUsage::unreported()
                },
            })),
            Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
                exchange: ExchangeFacts::default(),
                message_id: None,
                reported_model: Some(ProviderReportedModel::new("fixture-model")),
                finish: CompletionFinish::ToolUse,
                content: vec![
                    AssistantPart::Text("Repository task complete.".to_owned()),
                    AssistantPart::ToolCall(signalbox_model_runtime::ToolCallProposal {
                        id: signalbox_model_runtime::ToolCallId::new("goal-completion"),
                        name: signalbox_model_runtime::ToolName::new("goal_declare"),
                        arguments_json: r#"{"transition":"achieved"}"#.to_owned(),
                    }),
                ],
                usage: TokenUsage::default(),
            })),
            completed_script(
                "fixture-model",
                "Repository task complete.",
                TokenUsage::default(),
            ),
        ],
        [100, 100],
    );
    let ordinary_probe = ordinary.clone();
    let provider = RuntimeModelCallProvider::new(ordinary, runtime_models.clone(), None);
    let (catalog, executor) = signalboxd::goal_declaration_test_tools(runtime.pool.clone())?;
    let calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("continuation-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog())
    .with_continuation_usage_limits(
        configuration.tool_continuation_usage_limits(&catalog.definitions())?,
    );
    let execution = signalboxd::WorkspaceInstructionPreparedExecution::new(
        PostgresProviderModelExecution::new(
            calls.clone(),
            InProcessAttemptDispatchGate::default(),
            provider.clone(),
            None,
        )
        .with_tool_loop(
            InProcessToolDispatchGate::default(),
            catalog.clone(),
            executor,
        ),
        signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
    );
    let (nudge, mut work_source) = InProcessEligibilityWorkSource::with_options(
        NoCompactionSweep,
        None,
        std::num::NonZeroUsize::new(1),
    );
    let compaction =
        compaction.with_repository_watch_continuation(nudge, InProcessToolDispatchGate::default());
    let pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        calls,
        provider,
        catalog,
        runtime_models.clone(),
        configuration.clone(),
        Arc::new(RuntimeContextCompactionModel::new(
            ScriptedModel::following([]),
            runtime_models,
        )),
        HeapAllocatedExecution(execution),
    )
    .with_reported_usage_compaction(compaction.clone())
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        runtime.pool.clone(),
        None,
        Vec::new(),
    ));
    let disposition = signalboxd::PostgresGoalPassDisposition::new(
        runtime.pool.clone(),
        configuration,
        runtime.eligibility_nudge.clone(),
        signalboxd::GoalModeNumericBounds::new(None, None, None, None, None),
    );
    let mut pass = signalbox_application::GoalAwareEligibilityPass::new(pass, disposition);
    tokio::spawn(pass.run(session)).await??;
    for _ in 0..2 {
        let hint = timeout(Duration::from_secs(5), work_source.next()).await??;
        assert_eq!(hint, session);
        tokio::spawn(pass.run(hint)).await??;
    }
    assert_eq!(ordinary_probe.prepared_operations().len(), 3);
    assert_eq!(probe.received_operations().len(), 1);
    assert_eq!(
        probe.received_operations()[0].resolved_target.as_str(),
        "fixture-model",
        "compaction uses the direct model frozen by goal admission"
    );
    let repository = signalbox_persistence::goal::GoalRepository::new(runtime.pool.clone());
    let goal = repository
        .load_goal(session)
        .await?
        .expect("commissioned goal");
    let successor = repository
        .load_current_goal_turn(session, goal.current().generation())
        .await?
        .expect("the same turn remains in the commissioned lineage");
    assert_eq!(successor, original);
    assert_eq!(
        goal.events().len(),
        2,
        "commission followed directly by achievement"
    );
    assert!(
        matches!(
            goal.current().state(),
            signalbox_domain::GoalState::Achieved { .. }
        ),
        "{goal:?}"
    );
    let lifecycle = signalbox_persistence::session_lifecycle::SessionLifecycleRepository::new(
        runtime.pool.clone(),
    )
    .load(session)
    .await?
    .expect("held session lifecycle");
    assert!(
        matches!(
            lifecycle.state(),
            signalbox_domain::SessionLifecycleState::Terminal {
                outcome: signalbox_domain::SessionTerminalOutcome::AchievedDeclared
            }
        ),
        "{lifecycle:?}"
    );
    runtime.stop().await
}

// The composed execution future is large in an unoptimized integration binary.
#[derive(Clone)]
struct HeapAllocatedExecution<E>(E);

impl<E: signalboxd::ActivatedTurnExecution> signalboxd::ActivatedTurnExecution
    for HeapAllocatedExecution<E>
{
    type Error = E::Error;

    fn execute(
        &self,
        activated: Box<signalbox_domain::ActivatedTurn>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        Box::pin(self.0.execute(activated))
    }

    fn resume_active_with_observer(
        &self,
        session: SessionId,
        observe: Arc<dyn Fn(TurnId) + Send + Sync>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        Box::pin(self.0.resume_active_with_observer(session, observe))
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn interactive_compaction_preserves_its_active_turn() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let (session, turn) =
        exhausted_continuation(&runtime, ContinuationSession::Interactive).await?;
    let summary = ScriptedModel::single(completed_script(
        "fixture-model",
        "Continue the requested task.",
        TokenUsage::default(),
    ));
    let probe = summary.clone();
    let compaction = continuation_compaction(&runtime, summary)?;
    compaction.compact_if_needed(session, None).await?;
    compaction.compact_if_needed(session, None).await?;
    assert_eq!(probe.received_operations().len(), 1);
    let states: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT turn_id, state_kind FROM turn_lifecycle WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_all(&runtime.pool)
            .await?;
    assert_eq!(states, vec![(turn.into_uuid(), String::from("active"))]);
    runtime.stop().await
}

struct NoCompactionSweep;
impl EligibilitySweep for NoCompactionSweep {
    type Error = std::convert::Infallible;
    async fn find_sessions(&mut self) -> Result<EligibilitySweepBatch, Self::Error> {
        std::future::pending().await
    }
}

async fn resume_compaction_checkpoint(
    runtime: &RunningRuntime,
    session: SessionId,
    turn: TurnId,
    window: u64,
) -> Result<signalbox_application::PrepareToolContinuationOutcome, Box<dyn Error>> {
    let configuration = support::parse_model_configuration(&MODEL_CONFIGURATION.replace(
        "context_window_tokens = 200000",
        &format!("context_window_tokens = {window}"),
    ))?;
    let (catalog, _) = signalboxd::goal_declaration_test_tools(runtime.pool.clone())?;
    let producing: Uuid = sqlx::query_scalar(
        "SELECT active_tool_round_call_id FROM turn_lifecycle WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    let calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("continuation-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog())
    .with_continuation_usage_limits(
        configuration.tool_continuation_usage_limits(&catalog.definitions())?,
    );
    Ok(calls
        .tool_loop_repository()
        .prepare_continuation(
            session,
            turn,
            ModelCallId::from_uuid(producing),
            signalbox_application::ToolContinuationIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                ModelCallId::from_uuid(Uuid::now_v7()),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    TurnId::from_uuid(Uuid::now_v7()),
                )
            },
        )
        .await?)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn failed_or_refused_compaction_closes_the_active_checkpoint() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let failures = [
        TerminalEvidence::ProviderError(ProviderErrorEvidence {
            credential_recovery: None,
            exchange: ExchangeFacts::default(),
            reported_model: None,
            kind: ProviderErrorKind::Unrecognized,
            non_acceptance_proven: true,
            native: NativeErrorFacts::default(),
            usage: TokenUsage::unreported(),
        }),
        TerminalEvidence::Refused(signalbox_model_runtime::RefusalEvidence {
            reason: signalbox_model_runtime::RefusalReason::Unspecified,
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: None,
            content: Vec::new(),
            usage: TokenUsage::unreported(),
            retained_input_tokens: None,
            retained_output_tokens: None,
        }),
    ];
    for failure in failures {
        let (session, turn) =
            exhausted_continuation(&runtime, ContinuationSession::Interactive).await?;
        admit_compaction_steering(&runtime, session, turn, 300).await?;
        let summary = ScriptedModel::single(Script::delivering(failure));
        let probe = summary.clone();
        let compaction = continuation_compaction(&runtime, summary)?;
        assert!(compaction.compact_if_needed(session, None).await.is_err());
        assert!(matches!(
            resume_compaction_checkpoint(&runtime, session, turn, 200_000).await?,
            signalbox_application::PrepareToolContinuationOutcome::ContextCompactionFailed(_)
        ));
        assert_eq!(probe.received_operations().len(), 1);
        let lifecycle: (String, Option<Uuid>) = sqlx::query_as(
            "SELECT state_kind, compaction_frontier_id FROM turn_lifecycle WHERE turn_id = $1",
        )
        .bind(turn.into_uuid())
        .fetch_one(&runtime.pool)
        .await?;
        assert_eq!(lifecycle, (String::from("terminal"), None));
        assert_eq!(
            signalbox_persistence::goal::GoalRepository::new(runtime.pool.clone())
                .unchargeable_automatic_resume_turns(session, &[turn])
                .await?
                .as_ref(),
            &[turn],
            "daemon-owned compaction failure does not spend the goal resume budget"
        );
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'queued'",
        )
        .bind(session.into_uuid())
        .fetch_one(&runtime.pool)
        .await?;
        assert_eq!(
            queued, 1,
            "pending steering becomes eligible after failure closure"
        );
    }
    runtime.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn post_compaction_headroom_includes_pending_steering() -> Result<(), Box<dyn Error>> {
    assert_post_compaction_headroom(false).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn irreducible_active_compaction_releases_pending_steering() -> Result<(), Box<dyn Error>> {
    assert_post_compaction_headroom(true).await
}

async fn assert_post_compaction_headroom(irreducible: bool) -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let (session, turn) =
        exhausted_continuation(&runtime, ContinuationSession::Interactive).await?;
    // Forty summary bytes, 300 steering bytes and 256 output tokens leave four
    // tokens at a 600-token boundary. The complete request still cannot fit.
    let summary = ScriptedModel::following(
        (if irreducible {
            vec![1]
        } else {
            vec![80, 40, 20]
        })
        .into_iter()
        .map(|bytes| {
            completed_script(
                "fixture-model",
                &"s".repeat(bytes),
                TokenUsage::unreported(),
            )
        }),
    );
    let probe = summary.clone();
    let compaction = continuation_compaction(&runtime, summary)?;
    compaction.compact_if_needed(session, None).await?;
    admit_compaction_steering(&runtime, session, turn, if irreducible { 600 } else { 300 }).await?;
    let outcome = resume_compaction_checkpoint(&runtime, session, turn, 600).await?;
    if irreducible {
        assert!(matches!(
            outcome,
            signalbox_application::PrepareToolContinuationOutcome::ContextCompactionFailed(_)
        ));
    } else {
        assert!(matches!(
            outcome,
            signalbox_application::PrepareToolContinuationOutcome::ContextCompactionRequired(_)
        ));
        compaction.compact_if_needed(session, None).await?;
        assert!(matches!(
            resume_compaction_checkpoint(&runtime, session, turn, 600).await?,
            signalbox_application::PrepareToolContinuationOutcome::ContextCompactionRequired(_)
        ));
        compaction.compact_if_needed(session, None).await?;
        assert!(matches!(
            resume_compaction_checkpoint(&runtime, session, turn, 200_000).await?,
            signalbox_application::PrepareToolContinuationOutcome::Checkpointed(_)
        ));
    }
    assert_eq!(
        probe.received_operations().len(),
        if irreducible { 1 } else { 3 }
    );
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input WHERE session_id = $1 AND disposition_kind = 'pending_steering'")
        .bind(session.into_uuid()).fetch_one(&runtime.pool).await?;
    assert_eq!(pending, 0);
    runtime.stop().await
}

async fn admit_compaction_steering(
    runtime: &RunningRuntime,
    session: SessionId,
    turn: TurnId,
    bytes: usize,
) -> Result<(), Box<dyn Error>> {
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request_version(
            ProtocolVersion::One,
            50,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id: CanonicalUuid::from_uuid(session.into_uuid()),
                content: UserInputContent::text("p".repeat(bytes)),
                expected_defaults_version: None,
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::Steer {
                    expected_active_turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                }),
            },
        )
        .await?;
    let response = response_within(&mut connection).await?;
    assert!(!matches!(response.message(), ServerMessage::Error { .. }));
    drop(connection);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn compacted_credential_wait_releases_into_a_new_tool_round() -> Result<(), Box<dyn Error>> {
    use signalbox_persistence::model_execution::{
        CredentialPoolRuntimeAction, CredentialPoolRuntimeExhaustion, CredentialPoolRuntimeMember,
        CredentialPoolRuntimePolicy,
    };
    let runtime = RunningRuntime::start().await?;
    let (session, turn) =
        exhausted_continuation(&runtime, ContinuationSession::Interactive).await?;
    let compaction = continuation_compaction(
        &runtime,
        ScriptedModel::single(completed_script(
            "fixture-model",
            "Retained conversation summary.",
            TokenUsage::unreported(),
        )),
    )?;
    compaction.compact_if_needed(session, None).await?;
    let (producing, member): (Uuid, String) = sqlx::query_as(
        "SELECT call.model_call_id, call.credential_reference FROM turn_lifecycle AS lifecycle
         JOIN model_call AS call ON call.model_call_id = lifecycle.active_tool_round_call_id
         WHERE lifecycle.turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    sqlx::query("INSERT INTO credential_pool_transient_exclusion (observation_model_call_id, credential_reference, cause_kind, reset_at) VALUES ($1,$2,'overloaded',transaction_timestamp() + interval '1 hour')")
        .bind(producing).bind(&member).execute(&runtime.pool).await?;
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let (catalog, executor) = signalboxd::goal_declaration_test_tools(runtime.pool.clone())?;
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(3)));
    let policy = CredentialPoolRuntimePolicy::new(
        "compaction-wait-pool".to_owned(),
        vec![CredentialPoolRuntimeMember::new(
            member,
            std::num::NonZeroU32::new(1).expect("positive fixture priority"),
        )],
        CredentialPoolRuntimeExhaustion::Park,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::Quarantine,
    );
    let calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("continuation-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog())
    .with_continuation_usage_limits(
        configuration.tool_continuation_usage_limits(&catalog.definitions())?,
    )
    .with_credential_pools(std::collections::HashMap::from([(target, policy)]));
    let outcome = calls
        .tool_loop_repository()
        .prepare_continuation(
            session,
            turn,
            ModelCallId::from_uuid(producing),
            signalbox_application::ToolContinuationIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                ModelCallId::from_uuid(Uuid::now_v7()),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            |_| panic!("no steering in the credential wait fixture"),
        )
        .await?;
    let signalbox_application::PrepareToolContinuationOutcome::CredentialWait(wait) = outcome
    else {
        panic!("the compacted turn parks while its credential is excluded");
    };
    let checkpoint: Option<Uuid> =
        sqlx::query_scalar("SELECT compaction_frontier_id FROM turn_lifecycle WHERE turn_id = $1")
            .bind(turn.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(checkpoint, None);
    sqlx::query("UPDATE credential_pool_transient_exclusion SET reset_at = transaction_timestamp() WHERE observation_model_call_id = $1")
        .bind(producing).execute(&runtime.pool).await?;
    sqlx::query(
        "UPDATE credential_availability_wait SET eligible = true WHERE wait_attempt_id = $1",
    )
    .bind(wait.attempt().into_uuid())
    .execute(&runtime.pool)
    .await?;
    let models = configuration.runtime_model_catalog();
    let ordinary = compaction::RecordingCountedScriptedModel::following(
        [
            Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
                exchange: ExchangeFacts::default(),
                message_id: None,
                reported_model: Some(ProviderReportedModel::new("fixture-model")),
                finish: CompletionFinish::ToolUse,
                content: vec![AssistantPart::ToolCall(
                    signalbox_model_runtime::ToolCallProposal {
                        id: signalbox_model_runtime::ToolCallId::new("after-credential-wait"),
                        name: signalbox_model_runtime::ToolName::new("goal_declare"),
                        arguments_json: r#"{"transition":"achieved"}"#.to_owned(),
                    },
                )],
                usage: TokenUsage::unreported(),
            })),
            completed_script(
                "fixture-model",
                "Turn complete after credential wait.",
                TokenUsage::unreported(),
            ),
        ],
        [100, 100],
    );
    let probe = ordinary.clone();
    let provider = RuntimeModelCallProvider::new(ordinary, models.clone(), None);
    let instructions =
        signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new());
    let execution = signalboxd::WorkspaceInstructionPreparedExecution::new(
        PostgresProviderModelExecution::new(
            calls.clone(),
            InProcessAttemptDispatchGate::default(),
            provider.clone(),
            None,
        )
        .with_tool_loop(
            InProcessToolDispatchGate::default(),
            catalog.clone(),
            executor,
        ),
        instructions.clone(),
    );
    let mut pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        calls,
        provider,
        catalog,
        models.clone(),
        configuration,
        Arc::new(RuntimeContextCompactionModel::new(
            ScriptedModel::following([]),
            models,
        )),
        HeapAllocatedExecution(execution),
    )
    .with_workspace_instructions(instructions);
    pass.run(session).await?;
    assert_eq!(probe.prepared_operations().len(), 2);
    let (state, disposition): (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind FROM turn_lifecycle WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        (state.as_str(), disposition.as_deref()),
        ("terminal", Some("completed"))
    );
    runtime.stop().await
}
