//! A repository-watch continuation terminalizes, compacts, and starts one successor.

use super::*;
use signalbox_domain::{
    CreateSession, ModuleDispatch, ProviderReportedTokenUsage, RepoWatchDispatchId,
    SessionCreationCause, SessionCreationProvenance, SessionOwnership, StartGate,
    TranscriptAncestry,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository, model_execution::ToolContinuationUsageLimit,
    submit_input::SubmitInputRepository,
};

async fn exhausted_continuation(
    runtime: &RunningRuntime,
    repository_watch: bool,
) -> Result<(SessionId, TurnId), Box<dyn Error>> {
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let session = SessionId::from_uuid(Uuid::now_v7());
    let provenance = if repository_watch {
        SessionCreationProvenance::module_dispatched(ModuleDispatch::RepositoryWatch {
            dispatch: RepoWatchDispatchId::from_uuid(Uuid::now_v7()),
        })
    } else {
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None)
    };
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::now_v7()),
        provenance,
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(1)),
        )),
    )
    .with_lifecycle(StartGate::Open, SessionOwnership::Owned, None)
    .prepare(session)
    .expect("the fixture creates a pathless owned session");
    CreateSessionRepository::new(runtime.pool.clone(), configuration.session_credential_pin())
        .handle(creation)
        .await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let wire_session = CanonicalUuid::from_uuid(session.into_uuid());
    let (_, wire_turn) = submit_first_input(
        &mut connection,
        wire_session,
        String::from("Finish the repository change"),
    )
    .await?;
    let turn = TurnId::from_uuid(wire_turn.into_uuid());
    let (calls, authorized, producing_call) =
        authorize_issued_model_call(&runtime.pool, wire_session).await?;
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

fn continuation_compaction(
    runtime: &RunningRuntime,
    summary: ScriptedModel<ModelCallId>,
) -> Result<ReportedUsageCompaction, Box<dyn Error>> {
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
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
async fn repository_watch_continuation_compacts_and_completes_one_successor()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let (session, original) = exhausted_continuation(&runtime, true).await?;
    let summary = ScriptedModel::single(completed_script(
        "fixture-model",
        "The repository change remains unfinished.",
        TokenUsage::default(),
    ));
    let probe = summary.clone();
    let compaction = continuation_compaction(&runtime, summary)?;
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
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
    let calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("continuation-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog());
    let execution = signalboxd::WorkspaceInstructionPreparedExecution::new(
        PostgresProviderModelExecution::new(
            calls.clone(),
            InProcessAttemptDispatchGate::default(),
            provider.clone(),
            None,
        ),
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
        execution,
    )
    .with_reported_usage_compaction(compaction.clone())
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        runtime.pool.clone(),
        None,
        Vec::new(),
    ));
    pass.run(session).await?;
    pass.run(session).await?;
    assert_eq!(ordinary_probe.prepared_operations().len(), 1);
    assert_eq!(probe.received_operations().len(), 1);
    let (successor, command): (Uuid, Uuid) = sqlx::query_as(
        "SELECT origin_turn_id, accepting_command_id FROM accepted_input WHERE session_id = $1 ORDER BY acceptance_position DESC LIMIT 1",
    ).bind(session.into_uuid()).fetch_one(&runtime.pool).await?;
    assert_ne!(successor, original.into_uuid());
    let recorded = SubmitInputRepository::new(runtime.pool.clone())
        .load(DurableCommandId::from_uuid(command))
        .await?
        .expect("the successor command reconstitutes");
    assert_eq!(recorded.command().actor(), Actor::Core);
    let states: Vec<(Uuid, String)> = sqlx::query_as("SELECT turn_id, terminal_disposition_kind FROM turn_lifecycle WHERE session_id = $1 ORDER BY acceptance_position")
        .bind(session.into_uuid()).fetch_all(&runtime.pool).await?;
    assert_eq!(
        states,
        vec![
            (original.into_uuid(), String::from("failed")),
            (successor, String::from("completed"))
        ]
    );
    let summaries: i64 = sqlx::query_scalar("SELECT count(*) FROM compact_session_command WHERE session_id = $1 AND result_kind = 'applied'")
        .bind(session.into_uuid()).fetch_one(&runtime.pool).await?;
    assert_eq!(summaries, 1);
    runtime.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn interactive_continuation_does_not_create_a_compaction_successor()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let (session, _) = exhausted_continuation(&runtime, false).await?;
    let summary = ScriptedModel::following([]);
    let probe = summary.clone();
    continuation_compaction(&runtime, summary)?
        .compact_if_needed(session, None)
        .await?;
    assert!(probe.received_operations().is_empty());
    let turns: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turn_lifecycle WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(turns, 1);
    runtime.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn failed_continuation_compaction_closes_the_successor_without_retrying()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let (session, original) = exhausted_continuation(&runtime, true).await?;
    let summary = ScriptedModel::single(Script::delivering(TerminalEvidence::ProviderError(
        ProviderErrorEvidence {
            exchange: ExchangeFacts::default(),
            reported_model: None,
            kind: ProviderErrorKind::Unrecognized,
            non_acceptance_proven: true,
            native: NativeErrorFacts::default(),
            usage: TokenUsage::unreported(),
        },
    )));
    let probe = summary.clone();
    let compaction = continuation_compaction(&runtime, summary)?;
    assert!(matches!(
        compaction.compact_if_needed(session, None).await,
        Err(ReportedUsageCompactionError::Compaction {
            cause_code: "context_compaction_model",
            ..
        })
    ));
    compaction.compact_if_needed(session, None).await?;
    compaction.compact_if_needed(session, None).await?;
    assert_eq!(probe.received_operations().len(), 1);
    let states: Vec<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_model_call_id FROM turn_lifecycle WHERE session_id = $1 AND turn_id <> $2",
    ).bind(session.into_uuid()).bind(original.into_uuid()).fetch_all(&runtime.pool).await?;
    assert_eq!(states, vec![(String::from("failed"), None)]);
    runtime.stop().await
}
