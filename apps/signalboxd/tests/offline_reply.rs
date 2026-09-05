#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

mod support;

use std::{error::Error, process::Command, time::Duration};

use signalbox_application::{
    ClassifyOperatorFailure, CorrelatedToolExecutorEvidence, CreateSessionOutcome,
    CreateSessionRequest, CreateSessionService, EligibilityNudge, GoalAwareEligibilityPass,
    GoalPassDisposition, InProcessAttemptDispatchGate, InProcessEligibilityWorkSource,
    InProcessToolDispatchGate, ModelCallCredentialReference, NoToolCatalog, OperatorFailureClass,
    SchedulerLoop, SchedulerLoopExit, StartEligibleTurnOutcome, StartEligibleTurnService,
    SubmitInputOutcome, SubmitInputRequest, SubmitInputService, ToolExecutionInvocation,
    ToolExecutor, UuidV7SessionIdGenerator, UuidV7StartEligibleTurnIdGenerator,
    UuidV7SubmitInputIdGenerator,
};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnActivationIdentities, ContextFrontierId, DeliveryRequest,
    DirectModelSelection, DurableCommandId, FailedModelCallTurnIdentities, Goal,
    GoalBlockProvenance, GoalBlockedReasonKind, GoalCommandResult, GoalEvent, GoalEventKind,
    GoalState, GoalStatement, GoalUserAction, GoalUserCommand, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, PerInputConfigurationChoices,
    ProviderModelIdentity, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    SubmitInputAppliedResult, SubmitInputResult, TurnAttemptId, TurnId, TurnTerminalCause,
    UserContent,
};
use signalbox_model_provider_runtime::{
    RuntimeModelCallProvider, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, ExchangeFacts, ProviderReportedModel,
    RefusalEvidence, Script, ScriptedModel, TerminalEvidence, TokenUsage,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels,
    goal::{GoalCommandHandlingOutcome, GoalExecutionFailureRecoveryCause, GoalRepository},
    goal_turn::GoalTurnCandidates,
    local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository,
    process_read::{ProcessReadRepository, ProcessTranscriptEntry},
    scheduler::PostgresEligibilitySweep,
    start_eligible_turn::{CommitCompactionFailurePreviewOutcome, StartEligibleTurnRepository},
    submit_input::SubmitInputRepository,
};
use signalbox_test_bin::test_bin_path;
use signalboxd::{
    ActivatedTurnExecution, ActivatedTurnPass, CONTEXT_COMPACTION_INPUT_DOES_NOT_FIT_NEED,
    FatalExecutionSupervisor, GoalModeNumericBounds, PostgresGoalPassDisposition,
    PostgresProviderModelExecution,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::time::timeout;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
/// The undated provider-model spelling the fixture deployment configures.
const CONFIGURED_PROVIDER_MODEL: &str = "claude-haiku-4-5";
/// The canonical dated form of that same family, as a provider echoes it.
const SERVED_PROVIDER_MODEL: &str = "claude-haiku-4-5-20251001";
const DATABASE_NAME: &str = "signalboxd_e2e";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const SCHEDULED_EXECUTION_FAILURE_NEED: &str = "The goal turn failed to execute and automatic resumption is scheduled. If the goal is still blocked here once resumption ends, it is waiting for an operator. Resolve the failed goal turn's execution condition, then resume the goal.";
const GOAL_MODEL_CONFIGURATION: &str = r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]


[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "Summarize faithfully."

[[models]]
selection_id = "00000000-0000-0000-0000-000000002001"
target_id = "00000000-0000-0000-0000-000000002004"
model_family = "anthropic"
provider_model = "claude-haiku-4-5"
max_output_tokens = 64
context_window_tokens = 200000
"#;

fn test_session_credential_pin() -> signalbox_persistence::SessionCredentialPin {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        ),
    ])
    .expect("test credential pin is valid")
}

#[derive(Clone, Copy, Debug)]
struct UnexpectedToolExecutor;

#[derive(Clone, Copy, Debug)]
struct UnexpectedToolExecution;

impl std::fmt::Display for UnexpectedToolExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("empty catalog dispatched a tool")
    }
}

impl Error for UnexpectedToolExecution {}

impl ClassifyOperatorFailure for UnexpectedToolExecution {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

impl ToolExecutor for UnexpectedToolExecutor {
    type Error = UnexpectedToolExecution;

    async fn execute(
        &mut self,
        _invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        Err(UnexpectedToolExecution)
    }
}

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool, database_url))
}

async fn wait_for_terminal(pool: &PgPool, session: SessionId, turn: TurnId) {
    loop {
        let terminal: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND state_kind = 'terminal'
            )",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if terminal {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_execution_failure_block(pool: &PgPool, session: SessionId) {
    loop {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM goal_event
                 WHERE session_id = $1
                   AND event_kind = 'blocked'
                   AND blocked_reason = 'execution_failure'
            )",
        )
        .bind(session.into_uuid())
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if blocked {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn goal_completion_script() -> Script {
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new(SERVED_PROVIDER_MODEL)),
        finish: CompletionFinish::EndTurn,
        content: vec![AssistantPart::Text(String::from(
            "first goal turn completed",
        ))],
        usage: TokenUsage::unreported(),
    }))
}

fn goal_refusal_script() -> Script {
    Script::delivering(TerminalEvidence::Refused(RefusalEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new(SERVED_PROVIDER_MODEL)),
        content: Vec::new(),
        usage: TokenUsage::unreported(),
    }))
}

fn goal_statement(value: &str) -> GoalStatement {
    GoalStatement::try_new(value.to_owned()).expect("fixture goal statement is admitted")
}

fn goal_turn_candidates(value: u128) -> GoalTurnCandidates {
    GoalTurnCandidates::new(
        AcceptedInputId::from_uuid(Uuid::from_u128(value)),
        TurnId::from_uuid(Uuid::from_u128(value + 1)),
    )
}

#[track_caller]
fn assert_goal_command_applied(outcome: GoalCommandHandlingOutcome) {
    let GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_)) = outcome else {
        panic!("fixture goal command must apply");
    };
}

#[track_caller]
fn assert_execution_failure_blocked(goal: &Goal) {
    let GoalState::Blocked { reason, need } = goal.current().state() else {
        panic!("fixture goal must be blocked");
    };
    assert_eq!(*reason, GoalBlockedReasonKind::ExecutionFailure);
    // The first failure of a run is under the automatic-resumption budget, so
    // the need text states the scheduled attempt before the operator repair
    // every execution-failure need carries.
    assert_eq!(need.as_str(), SCHEDULED_EXECUTION_FAILURE_NEED);
}

#[track_caller]
fn execution_failure_turn(goal: &Goal) -> TurnId {
    let Some(GoalEventKind::Blocked {
        block: GoalBlockProvenance::ExecutionFailure { provenance },
        ..
    }) = goal.events().last().map(GoalEvent::kind)
    else {
        panic!("fixture goal must end with scheduler failure provenance");
    };
    provenance.turn()
}

/// S01 / S02 / INV-014 / INV-015: the complete offline
/// chain creates a session, submits input, lets the scheduler activate it,
/// invokes the application provider port, and atomically persists the exact
/// selection, resolved target, consumed frontier, Prepared-to-InFlight
/// checkpoint sequence, assistant reply, and terminal lifecycle facts.
/// INV-026: the bridge receives a one-action runtime script, so any repeated
/// physical interaction exhausts the script and fails the test.
/// S20: the fixture configures an undated provider-model spelling while the
/// scripted response echoes that family's canonical dated form, so the chain
/// also proves the provider-target normalization law of
/// docs/spec/model-call-execution.md end to end: the call completes and the
/// supervisor never raises a fatal signal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_s02_inv014_inv015_runtime_bridge_persists_scripted_assistant_reply()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x2001));
    let mut create = CreateSessionService::new(
        UuidV7SessionIdGenerator,
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(created) = create
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x2002)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?
    else {
        panic!("the unique fixture command must create its session")
    };
    let session = created.session();

    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
    let tool_dispatch_gate = InProcessToolDispatchGate::default();
    let mut submit = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        SubmitInputRepository::new(pool.clone()),
        nudge,
        tool_dispatch_gate.clone(),
    );
    let submitted_content = UserContent::try_text(String::from("offline user request"))
        .expect("fixture user content is admitted");
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x2003)),
            session,
            submitted_content.clone(),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )?)
        .await?
    else {
        panic!("the unique fixture input must create queued origin work")
    };
    let turn = origin.turn();

    let provider_identity = ProviderModelIdentity::from_uuid(Uuid::from_u128(0x2004));
    let target = ResolvedProviderTarget::naming(provider_identity);
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("one fixture target definition is unique");
    let runtime_models =
        RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
            target,
            String::from(CONFIGURED_PROVIDER_MODEL),
            64,
            200_000,
        )
        .expect("fixture runtime definition is valid")])
        .expect("one fixture runtime target is unique");
    let assistant_reply = String::from("offline assistant reply");
    let runtime = ScriptedModel::single(Script::delivering(TerminalEvidence::Completed(
        CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(SERVED_PROVIDER_MODEL)),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(assistant_reply.clone())],
            usage: TokenUsage::unreported(),
        },
    )));
    let provider = RuntimeModelCallProvider::new(runtime, runtime_models, None);
    let credential_reference = ModelCallCredentialReference::new("scripted-test");
    let (execution, fatal_execution) = FatalExecutionSupervisor::new(
        PostgresProviderModelExecution::new(
            PostgresModelCallRepository::new(
                pool.clone(),
                targets.clone(),
                credential_reference.clone(),
            ),
            InProcessAttemptDispatchGate::default(),
            provider,
            None,
        )
        .with_tool_loop(tool_dispatch_gate, NoToolCatalog, UnexpectedToolExecutor)
        .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
            pool.clone(),
            None,
            Vec::new(),
        )),
    );
    let pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(pool.clone()),
        ),
        execution,
    );
    let mut scheduler = SchedulerLoop::new(work_source, pass);
    let observation_pool = pool.clone();
    let fatal_shutdown = fatal_execution.clone();
    let shutdown = async move {
        tokio::select! {
            () = wait_for_terminal(&observation_pool, session, turn) => {}
            () = fatal_shutdown.wait() => {}
        }
    };
    assert_eq!(
        timeout(Duration::from_secs(10), scheduler.run_until(shutdown)).await?,
        SchedulerLoopExit::Shutdown
    );
    assert!(
        !fatal_execution.is_triggered(),
        "post-activation execution failure must stop this isolated scheduler"
    );

    let transcript = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the fixture session has a transcript");
    let [user_entry, assistant_entry, completed_entry] = transcript.entries() else {
        panic!("the completed fixture transcript has exactly three entries");
    };
    let ProcessTranscriptEntry::User {
        content: persisted_content,
        ..
    } = user_entry
    else {
        panic!("the first transcript entry must be user content: {user_entry:?}");
    };
    assert_eq!(persisted_content, &submitted_content);
    let ProcessTranscriptEntry::Assistant {
        content: persisted_reply,
        ..
    } = assistant_entry
    else {
        panic!("the second transcript entry must be assistant content: {assistant_entry:?}");
    };
    assert_eq!(persisted_reply, &assistant_reply);
    let ProcessTranscriptEntry::TurnCompleted {
        turn: completed_turn,
        ..
    } = completed_entry
    else {
        panic!("the third transcript entry must complete the turn: {completed_entry:?}");
    };
    assert_eq!(*completed_turn, turn);

    let terminal_shape: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM turn_lifecycle
              WHERE session_id = $1
                AND turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'),
            (SELECT count(*) FROM turn_attempt
              WHERE session_id = $1
                AND turn_id = $2
                AND state_kind = 'ended'
                AND end_disposition = 'turn_completed'),
            (SELECT count(*) FROM model_call
              WHERE session_id = $1
                AND turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed')",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal_shape, (1, 1, 1));

    let call_provenance: (Uuid, String, Option<Uuid>, Uuid, Uuid, Uuid) = sqlx::query_as(
        "SELECT call.model_call_id,
                call.selection_kind,
                call.direct_model_selection_id,
                call.resolved_provider_model_identity_id,
                call.context_frontier_id,
                turn.starting_frontier_id
           FROM model_call AS call
           JOIN turn_lifecycle AS turn
             ON turn.session_id = call.session_id
            AND turn.turn_id = call.turn_id
          WHERE call.session_id = $1
            AND call.turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(call_provenance.1, "direct");
    assert_eq!(call_provenance.2, Some(selection.into_uuid()));
    assert_eq!(call_provenance.3, provider_identity.into_uuid());
    assert_eq!(call_provenance.4, call_provenance.5);

    let transition_sequence = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT transition.call_state_kind,
                transition.terminal_disposition_kind
           FROM model_call_transition_outbox_event AS transition
          WHERE transition.session_id = $1
            AND transition.turn_id = $2
            AND transition.model_call_id = $3
          ORDER BY transition.event_sequence",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(call_provenance.0)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        transition_sequence,
        vec![
            (String::from("prepared"), None),
            (String::from("in_flight"), None),
            (String::from("terminal"), Some(String::from("completed"))),
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Runs an owned goal through a completed turn and unsuccessful successor, or
/// releases its activated first turn before that turn finishes unsuccessfully.
async fn goal_failure_block_after_success(
    ownership: signalbox_domain::SessionOwnership,
) -> Result<(ContainerAsync<Postgres>, PgPool, Goal), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let configuration = support::parse_model_configuration(GOAL_MODEL_CONFIGURATION)?;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x2001));
    let mut create = CreateSessionService::new(
        UuidV7SessionIdGenerator,
        CreateSessionRepository::new(pool.clone(), configuration.session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(created) = create
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x2101)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?
    else {
        panic!("the unique fixture command must create its session")
    };
    let session = created.session();
    let first_turn = goal_turn_candidates(0x2201);
    let goal_repository = GoalRepository::new(pool.clone());
    let attached = goal_repository
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0x2102)),
                session,
                GoalUserAction::Attach(goal_statement("finish the commissioned task")),
            ),
            Some(first_turn),
            |_| None,
        )
        .await?;
    assert_goal_command_applied(attached);
    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
    let _ = nudge.nudge(session);
    let tool_dispatch_gate = InProcessToolDispatchGate::default();
    let runtime = match ownership {
        signalbox_domain::SessionOwnership::Owned => {
            ScriptedModel::following([goal_completion_script(), goal_refusal_script()])
        }
        signalbox_domain::SessionOwnership::Unmonitored => {
            ScriptedModel::following([goal_refusal_script()])
        }
    };
    let provider =
        RuntimeModelCallProvider::new(runtime.clone(), configuration.runtime_model_catalog(), None);
    let credential_reference = ModelCallCredentialReference::new("scripted-goal-test");
    let (execution, fatal_execution) = FatalExecutionSupervisor::new(
        PostgresProviderModelExecution::new(
            PostgresModelCallRepository::new(
                pool.clone(),
                configuration.target_catalog(),
                credential_reference,
            ),
            InProcessAttemptDispatchGate::default(),
            provider,
            None,
        )
        .with_tool_loop(tool_dispatch_gate, NoToolCatalog, UnexpectedToolExecutor)
        .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
            pool.clone(),
            None,
            Vec::new(),
        )),
    );
    let disposition = PostgresGoalPassDisposition::new(
        pool.clone(),
        configuration,
        nudge,
        GoalModeNumericBounds::new(None, None, None, None, None),
    );
    match ownership {
        signalbox_domain::SessionOwnership::Owned => {
            let activated_pass = ActivatedTurnPass::new(
                StartEligibleTurnService::new(
                    UuidV7StartEligibleTurnIdGenerator,
                    StartEligibleTurnRepository::new(pool.clone()),
                ),
                execution,
            );
            let pass = GoalAwareEligibilityPass::new(activated_pass, disposition);
            let mut scheduler = SchedulerLoop::new(work_source, pass);
            let observation_pool = pool.clone();
            let fatal_shutdown = fatal_execution.clone();
            let shutdown = async move {
                tokio::select! {
                    () = wait_for_execution_failure_block(&observation_pool, session) => {}
                    () = fatal_shutdown.wait() => {}
                }
            };
            assert_eq!(
                timeout(Duration::from_secs(10), scheduler.run_until(shutdown)).await?,
                SchedulerLoopExit::Shutdown
            );
        }
        signalbox_domain::SessionOwnership::Unmonitored => {
            let mut activation = StartEligibleTurnService::new(
                UuidV7StartEligibleTurnIdGenerator,
                StartEligibleTurnRepository::new(pool.clone()),
            );
            let StartEligibleTurnOutcome::Activated(activated) =
                activation.execute(session).await?
            else {
                panic!("the owned goal turn must activate before release")
            };
            let released = signalbox_persistence::session_lifecycle_command::SessionLifecycleCommandRepository::new(pool.clone())
                .handle(
                    signalbox_domain::SessionLifecycleCommand::new(
                        DurableCommandId::from_uuid(Uuid::from_u128(0x2103)),
                        session,
                        signalbox_domain::SessionLifecycleOperation::Release,
                    ),
                    signalbox_domain::CommandPrincipal::Operator,
                )
                .await?;
            assert!(matches!(
                released,
                signalbox_persistence::session_lifecycle_command::SessionLifecycleCommandHandlingOutcome::Recorded(
                    signalbox_domain::SessionLifecycleCommandResult::Applied(_)
                )
            ));
            execution.execute(activated).await?;
            disposition.reconcile_success(session).await?;
        }
    }
    assert!(
        !fatal_execution.is_triggered(),
        "a provider refusal is a durable unsuccessful turn, not a fatal execution defect"
    );

    let goal = goal_repository
        .load_goal(session)
        .await?
        .expect("the attached goal remains readable");
    let goal_turn_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM goal_turn WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&pool)
            .await?;

    match ownership {
        signalbox_domain::SessionOwnership::Owned => {
            assert_eq!(goal_turn_count, 2);
            assert_eq!(runtime.received_operations().len(), 2);
            assert_ne!(first_turn.turn(), execution_failure_turn(&goal));
        }
        signalbox_domain::SessionOwnership::Unmonitored => {
            assert_eq!(goal_turn_count, 1);
            assert_eq!(runtime.received_operations().len(), 1);
            assert_eq!(first_turn.turn(), execution_failure_turn(&goal));
        }
    }

    Ok((container, pool, goal))
}

/// INV-048: a completed goal turn is followed without user input, and an
/// unsuccessful successor blocks with scheduler provenance without a retry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_inv048_success_continues_and_unsuccessful_turn_blocks_without_retry()
-> Result<(), Box<dyn Error>> {
    let (container, pool, goal) =
        goal_failure_block_after_success(signalbox_domain::SessionOwnership::Owned).await?;
    assert_execution_failure_blocked(&goal);
    pool.close().await;
    drop(container);
    Ok(())
}

/// An unmonitored session is owed no automatic resumption, and its failure
/// block's need says so instead of promising one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unmonitored_sessions_failure_block_schedules_no_resumption()
-> Result<(), Box<dyn Error>> {
    let (container, pool, goal) =
        goal_failure_block_after_success(signalbox_domain::SessionOwnership::Unmonitored).await?;
    let GoalState::Blocked { need, .. } = goal.current().state() else {
        panic!("the unmonitored goal must be blocked");
    };
    assert_eq!(
        need.as_str(),
        "The goal turn failed to execute and the session is unmonitored, so no automatic resumption is scheduled. Resolve the failed goal turn's execution condition, then resume the goal."
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: a goal turn whose durable recovery cause requires an operator is
/// parked by the shared resume planner, not only by the direct disposition
/// callback that reads the cause.
///
/// This drives the sequence that reaches `reconcile_success` with the cause
/// already recorded: the turn terminalizes as a call-free compaction failure
/// writing its `goal_execution_failure_recovery` row, the direct
/// `block_execution_failure` callback never runs — which is what a daemon
/// restart between the failing commit and the disposition future does — and the
/// next pass reconciles the still-undisposed terminal turn. The appended block
/// must carry the operator-required need, because planning it from block
/// provenance alone armed a resume into the same impossible compaction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_inv048_reconciled_success_parks_a_durably_non_resumable_failure()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let configuration = support::parse_model_configuration(GOAL_MODEL_CONFIGURATION)?;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x2001));
    let mut create = CreateSessionService::new(
        UuidV7SessionIdGenerator,
        CreateSessionRepository::new(pool.clone(), configuration.session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(created) = create
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x2301)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?
    else {
        panic!("the unique fixture command must create its session")
    };
    let session = created.session();
    let attached_turn = goal_turn_candidates(0x2401);
    let goal_repository = GoalRepository::new(pool.clone());
    let attached = goal_repository
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0x2302)),
                session,
                GoalUserAction::Attach(goal_statement("finish the commissioned task")),
            ),
            Some(attached_turn),
            |_| None,
        )
        .await?;
    assert_goal_command_applied(attached);

    let activation = StartEligibleTurnRepository::new(pool.clone());
    let preview = activation
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x2501)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x2502)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x2503)),
                TurnAttemptId::from_uuid(Uuid::from_u128(0x2504)),
            ),
        )
        .await?
        .expect("the queued goal turn has an activation preview");
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        DirectModelSelection::from_uuid(Uuid::from_u128(0x2601)),
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(0x2602))),
    )])
    .expect("one fixture target forms a catalog");
    let closure = activation
        .commit_compaction_failure_preview(
            preview,
            &PostgresModelCallRepository::new(
                pool.clone(),
                targets,
                ModelCallCredentialReference::new("compaction-failure-test-provider"),
            ),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x2701)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x2702)),
            ),
            TurnTerminalCause::ContextCompactionWall,
            Some(GoalExecutionFailureRecoveryCause::ContextCompactionInputDoesNotFit),
        )
        .await?;

    assert_eq!(
        closure,
        CommitCompactionFailurePreviewOutcome::Failed(attached_turn.turn())
    );
    assert_eq!(
        goal_repository
            .execution_failure_recovery_cause(session, attached_turn.turn())
            .await?,
        Some(GoalExecutionFailureRecoveryCause::ContextCompactionInputDoesNotFit)
    );

    let (nudge, _work_source) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
    PostgresGoalPassDisposition::new(
        pool.clone(),
        configuration,
        nudge,
        GoalModeNumericBounds::new(None, None, None, None, None),
    )
    .reconcile_success(session)
    .await?;

    let goal = goal_repository
        .load_goal(session)
        .await?
        .expect("the attached goal remains readable");
    let GoalState::Blocked { reason, need } = goal.current().state() else {
        panic!("the reconciled terminal failure must block the goal")
    };

    assert_eq!(*reason, GoalBlockedReasonKind::ExecutionFailure);
    assert_eq!(need.as_str(), CONTEXT_COMPACTION_INPUT_DOES_NOT_FIT_NEED);
    assert_eq!(execution_failure_turn(&goal), attached_turn.turn());

    pool.close().await;
    drop(container);
    Ok(())
}

/// The thin debug harness drives the same scheduler path and prints only the
/// terminal semantic transcript requested by its caller.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn debug_driver_prints_the_scripted_terminal_transcript() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let output = Command::new(test_bin_path!("signalbox-debug"))
        .env("SIGNALBOX_DEBUG_DATABASE_URL", database_url)
        .args(["driver user request", "driver assistant reply"])
        .output()?;
    assert!(
        output.status.success(),
        "debug driver must exit successfully"
    );
    assert_eq!(
        String::from_utf8(output.stdout)?,
        "user: \"driver user request\"\nassistant: \"driver assistant reply\"\nevent: turn_completed\n"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Invalid scripted output is rejected before the debug harness writes any
/// session or queued work to its database.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn debug_driver_rejects_invalid_reply_before_durable_writes() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let output = Command::new(test_bin_path!("signalbox-debug"))
        .env("SIGNALBOX_DEBUG_DATABASE_URL", database_url)
        .args(["valid user input", ""])
        .output()?;

    assert!(!output.status.success());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM session")
            .fetch_one(&pool)
            .await?,
        0
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Waits for the armed resumption to append its `resumed` event.
async fn resumed_goal(pool: &PgPool, session: SessionId) -> Result<Goal, Box<dyn Error>> {
    let repository = GoalRepository::new(pool.clone());
    let goal = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(goal) = repository.load_goal(session).await.ok().flatten()
                && matches!(
                    goal.events().last().map(GoalEvent::kind),
                    Some(GoalEventKind::Resumed { .. })
                )
            {
                return goal;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    Ok(goal)
}

/// Waits for adoption to persist the scheduled need before its delayed resume.
async fn armed_goal(pool: &PgPool, session: SessionId) -> Result<Goal, Box<dyn Error>> {
    let repository = GoalRepository::new(pool.clone());
    let goal = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(goal) = repository.load_goal(session).await.ok().flatten()
                && matches!(
                    goal.current().state(),
                    GoalState::Blocked { need, .. }
                        if need.as_str() == SCHEDULED_EXECUTION_FAILURE_NEED
                )
            {
                return goal;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    Ok(goal)
}

async fn adopt_session(pool: &PgPool, session: SessionId) -> Result<(), Box<dyn Error>> {
    let adopted =
        signalbox_persistence::session_lifecycle_command::SessionLifecycleCommandRepository::new(
            pool.clone(),
        )
        .handle(
            signalbox_domain::SessionLifecycleCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0x2104)),
                session,
                signalbox_domain::SessionLifecycleOperation::Adopt {
                    finish_condition: None,
                },
            ),
            signalbox_domain::CommandPrincipal::Operator,
        )
        .await?;
    assert!(matches!(
        adopted,
        signalbox_persistence::session_lifecycle_command::SessionLifecycleCommandHandlingOutcome::Recorded(
            signalbox_domain::SessionLifecycleCommandResult::Applied(_)
        )
    ));
    Ok(())
}

/// Adopting a session whose goal is blocked arms the resumption the
/// unmonitored block was not owed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn adopting_a_blocked_goal_arms_its_resumption() -> Result<(), Box<dyn Error>> {
    let (container, pool, goal) =
        goal_failure_block_after_success(signalbox_domain::SessionOwnership::Unmonitored).await?;
    let session = goal.session();
    adopt_session(&pool, session).await?;
    let configuration = support::parse_model_configuration(GOAL_MODEL_CONFIGURATION)?;
    let (nudge, _work_source) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
    PostgresGoalPassDisposition::new(
        pool.clone(),
        configuration,
        nudge,
        GoalModeNumericBounds::new(Some(Duration::ZERO), None, None, None, None),
    )
    .arm_blocked_goal_resumption(session);

    let resumed = resumed_goal(&pool, session).await?;

    assert_eq!(*resumed.current().state(), GoalState::Pursuing);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Adoption durably changes the unmonitored block's effective need before
/// the configured backoff elapses.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn adopting_a_blocked_goal_persists_its_scheduled_need() -> Result<(), Box<dyn Error>> {
    let (container, pool, goal) =
        goal_failure_block_after_success(signalbox_domain::SessionOwnership::Unmonitored).await?;
    let session = goal.session();
    adopt_session(&pool, session).await?;
    let configuration = support::parse_model_configuration(GOAL_MODEL_CONFIGURATION)?;
    let (nudge, _work_source) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
    PostgresGoalPassDisposition::new(
        pool.clone(),
        configuration,
        nudge,
        GoalModeNumericBounds::new(Some(Duration::from_secs(60)), None, None, None, None),
    )
    .arm_blocked_goal_resumption(session);

    let armed = armed_goal(&pool, session).await?;

    assert_execution_failure_blocked(&armed);

    pool.close().await;
    drop(container);
    Ok(())
}
