#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

mod support;

use signalbox_persistence::test_support::postgres::TestDatabase;
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
#[cfg(feature = "test-support")]
use signalbox_persistence::goal::GoalRecoveryProgress;
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    goal::{GoalCommandHandlingOutcome, GoalExecutionFailureRecoveryCause, GoalRepository},
    goal_turn::GoalTurnCandidates,
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
use sqlx::{PgPool, types::Uuid};
use tokio::time::timeout;

/// The undated provider-model spelling the fixture deployment configures.
const CONFIGURED_PROVIDER_MODEL: &str = "claude-haiku-4-5";
/// The canonical dated form of that same family, as a provider echoes it.
const SERVED_PROVIDER_MODEL: &str = "claude-haiku-4-5-20251001";
const SCHEDULED_EXECUTION_FAILURE_NEED: &str = "The goal turn failed to execute and automatic resumption is scheduled. If the goal is still blocked here once resumption ends, it is waiting for an operator. Resolve the failed goal turn's execution condition, then resume the goal.";
// numeric-bound: test - identifies the first durable recovery event
#[cfg(feature = "test-support")]
const FIRST_RECOVERY_EVENT_COUNT: i64 = 1;
// numeric-bound: test - identifies the second durable execution-failure block
#[cfg(feature = "test-support")]
const SECOND_FAILURE_EVENT_COUNT: i64 = 2;
// numeric-bound: test - counts the commissioned turn and its two successors
#[cfg(feature = "test-support")]
const RECOVERY_CYCLE_TURN_COUNT: i64 = 3;
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

async fn migrated_postgres() -> Result<(TestDatabase, PgPool, String), Box<dyn Error>> {
    signalbox_persistence::test_support::postgres::migrated_postgres(8).await
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

#[cfg(feature = "test-support")]
async fn wait_for_goal_recovery_count(
    repository: &GoalRepository,
    session: SessionId,
    count: fn(GoalRecoveryProgress) -> i64,
    expected: i64,
) -> Result<(), signalbox_persistence::goal::GoalRepositoryError> {
    loop {
        let progress = repository.recovery_progress(session).await?;
        if count(progress) >= expected {
            return Ok(());
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
        reason: signalbox_model_runtime::RefusalReason::Unspecified,
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new(SERVED_PROVIDER_MODEL)),
        content: Vec::new(),
        usage: TokenUsage::unreported(),
        retained_input_tokens: None,
        retained_output_tokens: None,
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

#[derive(Clone)]
struct FailOneSession<Execution> {
    inner: Execution,
    failed_session: SessionId,
}

#[derive(Debug)]
enum SelectiveExecutionFailure<Inner> {
    Injected,
    Inner(Inner),
}

impl<Inner: ClassifyOperatorFailure> ClassifyOperatorFailure for SelectiveExecutionFailure<Inner> {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Injected => OperatorFailureClass::CallerOrHubBug,
            Self::Inner(error) => error.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Injected => "injected_execution_failure",
            Self::Inner(error) => error.operator_failure_cause_code(),
        }
    }
}

impl<Execution: ActivatedTurnExecution> ActivatedTurnExecution for FailOneSession<Execution> {
    type Error = SelectiveExecutionFailure<Execution::Error>;

    fn execute(
        &self,
        activated: Box<signalbox_domain::ActivatedTurn>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let fail = activated.session() == self.failed_session;
        let operation = self.inner.execute(activated);
        async move {
            if fail {
                Err(SelectiveExecutionFailure::Injected)
            } else {
                operation.await.map_err(SelectiveExecutionFailure::Inner)
            }
        }
    }
}

async fn wait_for_operator_park(pool: &PgPool, session: SessionId) {
    let lifecycle =
        signalbox_persistence::session_lifecycle::SessionLifecycleRepository::new(pool.clone());
    loop {
        if lifecycle
            .load(session)
            .await
            .expect("lifecycle is readable")
            .is_some_and(|record| record.state().is_parked())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// the complete offline chain creates a session, submits input, lets the scheduler activate it,
/// invokes the application provider port, and atomically persists the exact selection, resolved
/// target, consumed frontier, Prepared-to-InFlight checkpoint sequence, assistant reply, and
/// terminal lifecycle facts. the bridge receives a one-action runtime script, so any repeated
/// physical interaction exhausts the script and fails the test. the fixture configures an undated
/// provider-model spelling while the scripted response echoes that family's canonical dated form,
/// so the chain also proves the provider-target normalization law of
/// docs/spec/model-call-execution.md end to end: the call completes and the supervisor never raises
/// a process-wide fatal signal while a second session fails after activation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn fatal_session_supervision_parks_its_cause_while_another_turn_completes()
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

    let CreateSessionOutcome::Applied(failed_created) = create
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?
    else {
        panic!("the second session is created");
    };
    let failed_session = failed_created.session();
    submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            failed_session,
            UserContent::try_text("fail this execution".to_owned())
                .expect("the fixture text is nonempty"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )?)
        .await?;

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
    let (execution, fatal_execution) = FatalExecutionSupervisor::new(FailOneSession {
        failed_session,
        inner: PostgresProviderModelExecution::new(
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
    });
    let reporter = execution.recovery_reporter();
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
            () = async {
                wait_for_terminal(&observation_pool, session, turn).await;
                wait_for_operator_park(&observation_pool, failed_session).await;
            } => {}
            () = fatal_shutdown.wait_for_process_recovery() => {}
        }
    };
    let scheduled = async {
        tokio::select! {
            result = scheduler.run_until(shutdown) => result,
            () = reporter.park_failed_sessions(pool.clone()) => panic!("supervision must remain running"),
        }
    };
    assert_eq!(
        timeout(Duration::from_secs(10), scheduled).await?,
        SchedulerLoopExit::Shutdown
    );
    let parked =
        signalbox_persistence::session_lifecycle::SessionLifecycleRepository::new(pool.clone())
            .load(failed_session)
            .await?
            .expect("failed session remains available");
    assert!(parked.state().is_parked());
    assert_eq!(
        parked.ownership(),
        signalbox_domain::SessionOwnership::Unmonitored
    );
    let cause = parked
        .supervision_failure()
        .expect("the operator sees durable failure evidence");
    assert_eq!(cause.class, OperatorFailureClass::CallerOrHubBug);
    assert_eq!(cause.cause_code, "injected_execution_failure");
    assert!(cause.pending);

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

#[cfg_attr(
    not(feature = "test-support"),
    allow(
        dead_code,
        reason = "recovery probes are read by the test-support scenario"
    )
)]
struct GoalFailureFixture<Pass, Probe> {
    container: TestDatabase,
    pool: PgPool,
    goal: Goal,
    scheduler: SchedulerLoop<InProcessEligibilityWorkSource<PostgresEligibilitySweep>, Pass>,
    operation_count: Probe,
    nudge: signalbox_application::InProcessEligibilityNudge,
    fatal: signalboxd::FatalExecutionSignal,
}

/// Runs an owned goal through a completed turn and unsuccessful successor, or
/// releases its activated first turn before that turn finishes unsuccessfully.
async fn goal_failure_block_after_success(
    ownership: signalbox_domain::SessionOwnership,
    failure: Script,
) -> Result<
    GoalFailureFixture<
        impl signalbox_application::EligibilityPass<Error: ClassifyOperatorFailure + Send + 'static>
        + Send,
        impl Fn() -> usize,
    >,
    Box<dyn Error>,
> {
    let runtime = match ownership {
        signalbox_domain::SessionOwnership::Owned => {
            ScriptedModel::following([goal_completion_script(), failure.clone(), failure])
        }
        signalbox_domain::SessionOwnership::Unmonitored => ScriptedModel::following([failure]),
    };
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
    let restart_nudge = nudge.clone();
    let _ = nudge.nudge(session);
    let tool_dispatch_gate = InProcessToolDispatchGate::default();
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
    let activated_pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(pool.clone()),
        ),
        execution.clone(),
    );
    let pass = GoalAwareEligibilityPass::new(activated_pass, disposition.clone());
    let mut scheduler = SchedulerLoop::new(work_source, pass);
    match ownership {
        signalbox_domain::SessionOwnership::Owned => {
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
    let goal_turn_count = GoalRepository::new(pool.clone())
        .recovery_progress(session)
        .await?
        .turns();

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

    Ok(GoalFailureFixture {
        container,
        pool,
        goal,
        scheduler,
        operation_count: move || runtime.received_operations().len(),
        nudge: restart_nudge,
        fatal: fatal_execution,
    })
}

/// a completed goal turn is followed without user input, and an
/// unsuccessful successor blocks with scheduler provenance without a retry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_success_continues_and_unsuccessful_turn_blocks_without_retry()
-> Result<(), Box<dyn Error>> {
    let GoalFailureFixture {
        container,
        pool,
        goal,
        ..
    } = goal_failure_block_after_success(
        signalbox_domain::SessionOwnership::Owned,
        goal_refusal_script(),
    )
    .await?;
    assert_execution_failure_blocked(&goal);
    pool.close().await;
    drop(container);
    Ok(())
}

/// Repeated reconciliation inventories re-arm one pending block; the resumed turn
/// blocks durably after another refusal. This exercises the reconciliation method.
#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repeated_reconciliation_resumes_a_blocked_goal_once() -> Result<(), Box<dyn Error>> {
    use std::sync::Arc;
    use tokio::sync::Barrier;

    let GoalFailureFixture {
        container,
        pool,
        goal,
        mut scheduler,
        operation_count,
        nudge,
        fatal,
    } = goal_failure_block_after_success(
        signalbox_domain::SessionOwnership::Owned,
        goal_refusal_script(),
    )
    .await?;
    let session = goal.session();
    assert_execution_failure_blocked(&goal);
    // The two spawned resumptions and this test release recovery together.
    let resume_barrier = Arc::new(Barrier::new(3));
    let reconciliation = PostgresGoalPassDisposition::new(
        pool.clone(),
        support::parse_model_configuration(GOAL_MODEL_CONFIGURATION)?,
        nudge,
        GoalModeNumericBounds::new(None, None, None, None, None),
    )
    .with_startup_resume_barrier(resume_barrier.clone());
    let first = reconciliation
        .reconcile_automatic_resumptions_after_restart()
        .await?;
    let repeated = reconciliation
        .reconcile_automatic_resumptions_after_restart()
        .await?;
    assert_eq!(first, usize::try_from(FIRST_RECOVERY_EVENT_COUNT)?);
    assert_eq!(repeated, usize::try_from(FIRST_RECOVERY_EVENT_COUNT)?);
    let repository = GoalRepository::new(pool.clone());
    assert_eq!(
        repository.recovery_progress(session).await?.resumptions(),
        0
    );
    timeout(Duration::from_secs(10), resume_barrier.wait()).await?;
    timeout(
        Duration::from_secs(10),
        wait_for_goal_recovery_count(
            &repository,
            session,
            GoalRecoveryProgress::resumptions,
            FIRST_RECOVERY_EVENT_COUNT,
        ),
    )
    .await??;

    let observation_pool = pool.clone();
    let fatal_shutdown = fatal.clone();
    let shutdown = async move {
        let observation_repository = GoalRepository::new(observation_pool);
        tokio::select! {
            result = wait_for_goal_recovery_count(
                &observation_repository,
                session,
                GoalRecoveryProgress::execution_failure_blocks,
                SECOND_FAILURE_EVENT_COUNT,
            ) => { result.expect("recovery progress remains readable"); }
            () = fatal_shutdown.wait() => {}
        }
    };
    assert_eq!(
        timeout(Duration::from_secs(10), scheduler.run_until(shutdown)).await?,
        SchedulerLoopExit::Shutdown
    );

    let recovered_goal = GoalRepository::new(pool.clone())
        .load_goal(session)
        .await?
        .expect("the recovered goal remains readable");
    let progress = repository.recovery_progress(session).await?;

    assert_execution_failure_blocked(&recovered_goal);
    assert_eq!(progress.resumptions(), FIRST_RECOVERY_EVENT_COUNT);
    assert_eq!(
        progress.execution_failure_blocks(),
        SECOND_FAILURE_EVENT_COUNT
    );
    assert_eq!(progress.turns(), RECOVERY_CYCLE_TURN_COUNT);
    assert_eq!(i64::try_from(operation_count())?, RECOVERY_CYCLE_TURN_COUNT);

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
    let GoalFailureFixture {
        container,
        pool,
        goal,
        ..
    } = goal_failure_block_after_success(
        signalbox_domain::SessionOwnership::Unmonitored,
        goal_refusal_script(),
    )
    .await?;
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

/// a goal turn whose durable recovery cause requires an operator is
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
async fn s_goal_reconciled_success_parks_a_durably_non_resumable_failure()
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

#[cfg(feature = "test-support")]
struct UnmonitoredGoalCompletion {
    database: TestDatabase,
    pool: PgPool,
    session: SessionId,
    first_turn: GoalTurnCandidates,
    disposition: PostgresGoalPassDisposition,
    nudge: signalbox_application::InProcessEligibilityNudge,
    work_source: InProcessEligibilityWorkSource<PostgresEligibilitySweep>,
}

/// Releases an active scripted goal turn, completes it unmonitored, and consumes
/// the initial scheduler sweep before returning the idle, pursuing goal.
#[cfg(feature = "test-support")]
async fn complete_released_goal_turn() -> Result<UnmonitoredGoalCompletion, Box<dyn Error>> {
    use signalbox_application::EligibilityWorkSource;

    let runtime = ScriptedModel::following([goal_completion_script()]);
    let (container, pool, _database_url) = migrated_postgres().await?;
    let configuration = support::parse_model_configuration(GOAL_MODEL_CONFIGURATION)?;
    // Selection identity declared by GOAL_MODEL_CONFIGURATION.
    const CONFIGURED_SELECTION: u128 = 0x2001;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(CONFIGURED_SELECTION));
    let mut create = CreateSessionService::new(
        UuidV7SessionIdGenerator,
        CreateSessionRepository::new(pool.clone(), configuration.session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(created) = create
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )?)
        .await?
    else {
        panic!("the unique fixture command must create its session")
    };
    let session = created.session();
    let first_turn = GoalTurnCandidates::new(
        AcceptedInputId::from_uuid(Uuid::now_v7()),
        TurnId::from_uuid(Uuid::now_v7()),
    );
    let goal_repository = GoalRepository::new(pool.clone());
    let attached = goal_repository
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                GoalUserAction::Attach(goal_statement("finish the commissioned task")),
            ),
            Some(first_turn),
            |_| None,
        )
        .await?;
    assert_goal_command_applied(attached);
    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (nudge, mut work_source) = InProcessEligibilityWorkSource::new(sweep);
    assert_eq!(work_source.next().await?, session);
    let tool_dispatch_gate = InProcessToolDispatchGate::default();
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
        nudge.clone(),
        GoalModeNumericBounds::new(None, None, None, None, None),
    );
    let mut activation = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = activation.execute(session).await? else {
        panic!("the owned goal turn must activate before release")
    };
    let released =
        signalbox_persistence::session_lifecycle_command::SessionLifecycleCommandRepository::new(
            pool.clone(),
        )
        .handle(
            signalbox_domain::SessionLifecycleCommand::new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
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
    assert_eq!(
        goal_repository
            .load_goal(session)
            .await?
            .expect("attached goal")
            .current()
            .state(),
        &GoalState::Pursuing,
    );
    assert_eq!(goal_repository.recovery_progress(session).await?.turns(), 1);

    assert!(!fatal_execution.is_triggered());
    Ok(UnmonitoredGoalCompletion {
        database: container,
        pool,
        session,
        first_turn,
        disposition,
        nudge,
        work_source,
    })
}

/// Adoption queues the successor of a turn completed after ownership release.
#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn adoption_continues_a_goal_completed_while_unmonitored() -> Result<(), Box<dyn Error>> {
    use signalbox_application::{EligibilityPass, EligibilityWorkSource};

    let UnmonitoredGoalCompletion {
        database: container,
        pool,
        session,
        first_turn,
        disposition,
        mut work_source,
        ..
    } = complete_released_goal_turn().await?;
    let goal_repository = GoalRepository::new(pool.clone());
    adopt_session(&pool, session).await?;
    disposition.arm_adopted_goal_resumption(session);
    let hint = timeout(Duration::from_secs(10), work_source.next()).await??;
    assert_eq!(hint, session, "adoption wakes the ordinary scheduler");
    let mut pass = GoalAwareEligibilityPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(pool.clone()),
        ),
        disposition.clone(),
    );
    pass.run(hint).await?;
    // Replaying adoption must leave the same queued successor.
    adopt_session(&pool, session).await?;
    disposition.reconcile_success(session).await?;
    let queued: Vec<Uuid> = sqlx::query_scalar(
        "SELECT turn_id FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'queued'",
    )
    .bind(session.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(queued.len(), 1, "adoption queues exactly one successor");
    assert_ne!(queued[0], first_turn.turn().into_uuid());
    assert_eq!(goal_repository.recovery_progress(session).await?.turns(), 2);
    pool.close().await;
    drop(container);
    Ok(())
}

/// Startup recovers an adoption committed before its in-process hook ran.
#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn startup_recovers_adoption_committed_before_its_scheduler_nudge()
-> Result<(), Box<dyn Error>> {
    use signalbox_application::{EligibilityPass, EligibilityWorkSource};

    let UnmonitoredGoalCompletion {
        database: container,
        pool,
        session,
        first_turn,
        disposition,
        work_source,
        ..
    } = complete_released_goal_turn().await?;
    adopt_session(&pool, session).await?;
    // No adoption hook runs before the old in-process scheduler is dropped.
    drop(disposition);
    drop(work_source);
    let (nudge, mut restarted_source) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
    let restarted_disposition = PostgresGoalPassDisposition::new(
        pool.clone(),
        support::parse_model_configuration(GOAL_MODEL_CONFIGURATION)?,
        nudge,
        GoalModeNumericBounds::new(None, None, None, None, None),
    );
    let hint = timeout(Duration::from_secs(10), restarted_source.next()).await??;
    assert_eq!(
        hint, session,
        "the initial sweep rediscovers the committed adoption without periodic sweeps"
    );
    let mut pass = GoalAwareEligibilityPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(pool.clone()),
        ),
        restarted_disposition,
    );
    pass.run(hint).await?;
    let queued: Vec<Uuid> = sqlx::query_scalar(
        "SELECT turn_id FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'queued'",
    )
    .bind(session.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        queued.len(),
        1,
        "startup queues exactly one successor after committed adoption"
    );
    assert_ne!(queued[0], first_turn.turn().into_uuid());
    assert_eq!(
        GoalRepository::new(pool.clone())
            .recovery_progress(session)
            .await?
            .turns(),
        2
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// Module adoption wakes a pursuing goal through the ordinary goal hook.
#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn module_adoption_continues_a_goal_completed_while_unmonitored() -> Result<(), Box<dyn Error>>
{
    use signalbox_application::{EligibilityPass, EligibilityWorkSource};
    use signalbox_module_repo_watch_v2::dispatch::{CommandSubmission, SessionCommandSink};
    use std::sync::Arc;

    let UnmonitoredGoalCompletion {
        database: container,
        pool,
        session,
        first_turn,
        disposition,
        nudge,
        mut work_source,
    } = complete_released_goal_turn().await?;
    let mut sink = signalboxd::repo_watch_dispatch::RepositoryWatchCommandSink {
        goal_resumption: disposition.clone(),
        checkout_runner: None,
        pool: pool.clone(),
        models: Arc::new(support::parse_model_configuration(
            GOAL_MODEL_CONFIGURATION,
        )?),
        eligibility_nudge: nudge,
        tool_dispatch_gate: InProcessToolDispatchGate::default(),
    };
    let command = signalbox_session_ownership::SessionCommand::lifecycle(
        signalbox_domain::SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            session,
            signalbox_domain::SessionLifecycleOperation::Adopt {
                finish_condition: None,
            },
        ),
    )
    .expect("the ownership seam admits adoption");
    assert!(matches!(
        sink.submit(command.clone()).await.expect("module adoption"),
        CommandSubmission::Accepted
    ));
    let hint = timeout(Duration::from_secs(10), work_source.next()).await??;
    assert_eq!(
        hint, session,
        "module adoption wakes the ordinary scheduler without periodic sweeps"
    );
    let mut pass = GoalAwareEligibilityPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(pool.clone()),
        ),
        disposition.clone(),
    );
    pass.run(hint).await?;
    assert!(matches!(
        sink.submit(command).await.expect("module adoption replay"),
        CommandSubmission::Accepted
    ));
    disposition.reconcile_success(session).await?;
    let queued: Vec<Uuid> = sqlx::query_scalar(
        "SELECT turn_id FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'queued'",
    )
    .bind(session.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        queued.len(),
        1,
        "module adoption queues exactly one successor"
    );
    assert_ne!(queued[0], first_turn.turn().into_uuid());
    assert_eq!(
        GoalRepository::new(pool.clone())
            .recovery_progress(session)
            .await?
            .turns(),
        2
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// Adopting a session whose goal is blocked arms the resumption the
/// unmonitored block was not owed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn adopting_a_blocked_goal_arms_its_resumption() -> Result<(), Box<dyn Error>> {
    let GoalFailureFixture {
        container,
        pool,
        goal,
        ..
    } = goal_failure_block_after_success(
        signalbox_domain::SessionOwnership::Unmonitored,
        goal_refusal_script(),
    )
    .await?;
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
    .arm_adopted_goal_resumption(session);

    let resumed = resumed_goal(&pool, session).await?;

    assert_eq!(*resumed.current().state(), GoalState::Pursuing);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Reads the input durably queued by the last resumption event.
async fn resumed_goal_input(pool: &PgPool, resumed: &Goal) -> Result<UserContent, Box<dyn Error>> {
    Ok(signalbox_persistence::test_support::goal_resumption_input(
        pool,
        resumed.session(),
        resumed
            .events()
            .last()
            .expect("resumed event exists")
            .ordinal(),
    )
    .await?)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn automatic_resume_persists_strategy_guidance_for_a_chargeable_failure()
-> Result<(), Box<dyn Error>> {
    let GoalFailureFixture {
        container: _container,
        pool,
        goal: blocked,
        ..
    } = goal_failure_block_after_success(
        signalbox_domain::SessionOwnership::Unmonitored,
        goal_refusal_script(),
    )
    .await?;
    let session = blocked.session();
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
    .arm_adopted_goal_resumption(session);

    let resumed = resumed_goal(&pool, session).await?;
    let input = resumed_goal_input(&pool, &resumed).await?;

    assert_eq!(
        input,
        UserContent::try_text(String::from(
            "Continue pursuing the commissioned goal. The preceding turn failed to execute. Inspect the durable session state and choose a different safe approach before repeating the failed operation."
        )).expect("guidance fixture is valid user content")
    );
    assert_ne!(
        input,
        UserContent::try_text(blocked.current().statement().as_str().to_owned())
            .expect("goal statement is valid user content")
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn automatic_resume_preserves_the_statement_for_an_exempt_provider_failure()
-> Result<(), Box<dyn Error>> {
    let overloaded = Script::delivering(TerminalEvidence::ProviderError(
        signalbox_model_runtime::ProviderErrorEvidence {
            exchange: ExchangeFacts::default(),
            reported_model: Some(ProviderReportedModel::new(SERVED_PROVIDER_MODEL)),
            kind: signalbox_model_runtime::ProviderErrorKind::Overloaded,
            non_acceptance_proven: false,
            native: signalbox_model_runtime::NativeErrorFacts::default(),
            usage: TokenUsage::unreported(),
        },
    ));
    let GoalFailureFixture {
        container: _container,
        pool,
        goal: blocked,
        ..
    } = goal_failure_block_after_success(
        signalbox_domain::SessionOwnership::Unmonitored,
        overloaded,
    )
    .await?;
    let session = blocked.session();
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
    .arm_adopted_goal_resumption(session);

    let resumed = resumed_goal(&pool, session).await?;
    let input = resumed_goal_input(&pool, &resumed).await?;

    assert_eq!(
        input,
        UserContent::try_text(blocked.current().statement().as_str().to_owned())
            .expect("goal statement is valid user content")
    );
    pool.close().await;
    Ok(())
}

/// Adoption durably changes the unmonitored block's effective need before
/// the configured backoff elapses.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn adopting_a_blocked_goal_persists_its_scheduled_need() -> Result<(), Box<dyn Error>> {
    let GoalFailureFixture {
        container,
        pool,
        goal,
        ..
    } = goal_failure_block_after_success(
        signalbox_domain::SessionOwnership::Unmonitored,
        goal_refusal_script(),
    )
    .await?;
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
    .arm_adopted_goal_resumption(session);

    let armed = armed_goal(&pool, session).await?;

    assert_execution_failure_blocked(&armed);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_successor_uses_the_reloaded_alias_definition() -> Result<(), Box<dyn Error>> {
    use signalbox_persistence::reload_configuration::{
        ReloadConfiguration, ReloadLookup, ReloadResult,
    };
    use signalboxd::configuration_reload::ConfigurationReload;
    let (_container, pool, _) = migrated_postgres().await?;
    // Distinct fixture selections expose reuse of the startup alias definition.
    let alias = signalbox_domain::ModelAlias::from_uuid(Uuid::from_u128(0x2003));
    let next_selection = Uuid::from_u128(0x2002);
    let mut source = GOAL_MODEL_CONFIGURATION.parse::<toml_edit::DocumentMut>()?;
    let credential = tempfile::NamedTempFile::new()?;
    source["credential_profiles"][0]["file"] =
        toml_edit::value(credential.path().to_str().expect("fixture credential path"));
    let example = include_str!("../../../config/signalboxd.example.toml")
        .parse::<toml_edit::DocumentMut>()?;
    source.insert("numeric_bounds", example["numeric_bounds"].clone());
    let models = source["models"].as_array_of_tables_mut().expect("models");
    let mut next_model = models.get(0).expect("first model").clone();
    next_model.insert("selection_id", toml_edit::value(next_selection.to_string()));
    next_model.insert(
        "target_id",
        toml_edit::value(Uuid::from_u128(0x2005).to_string()),
    );
    models.push(next_model);
    let mut aliases = toml_edit::ArrayOfTables::new();
    let mut definition = toml_edit::Table::new();
    definition.insert("alias_id", toml_edit::value(alias.as_uuid().to_string()));
    definition.insert(
        "selection_id",
        toml_edit::value(Uuid::from_u128(0x2001).to_string()),
    );
    aliases.push(definition);
    source.insert("aliases", toml_edit::Item::ArrayOfTables(aliases));
    let configuration = signalboxd::HubModelConfiguration::parse(&source.to_string())?;
    let files = tempfile::tempdir()?;
    let model_path = files.path().join("models.toml");
    let template_path = files.path().join("templates.toml");
    std::fs::write(&template_path, "version = 1\n")?;
    let reload = ConfigurationReload::new(
        pool.clone(),
        configuration.clone(),
        signalboxd::SessionTemplateConfiguration::default(),
        model_path.clone(),
        template_path,
        None,
    )
    .map_err(|error| std::io::Error::other(format!("reload fixture: {error:?}")))?;
    let mut create = CreateSessionService::new(
        UuidV7SessionIdGenerator,
        CreateSessionRepository::new(pool.clone(), configuration.session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(created) = create
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Alias(alias)),
        )?)
        .await?
    else {
        panic!("fixture session is created")
    };
    let session = created.session();
    let first = goal_turn_candidates(0x2201);
    let repository = GoalRepository::new(pool.clone());
    assert_goal_command_applied(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    DurableCommandId::from_uuid(Uuid::now_v7()),
                    session,
                    GoalUserAction::Attach(goal_statement("continue through the updated alias")),
                ),
                Some(first),
                |alias| configuration.resolve_alias(alias),
            )
            .await?,
    );
    let (nudge, _work) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
    let disposition = PostgresGoalPassDisposition::new(
        pool.clone(),
        configuration.clone(),
        nudge,
        GoalModeNumericBounds::new(None, None, None, None, None),
    )
    .with_configuration_reload(reload.clone());
    let provider = RuntimeModelCallProvider::new(
        ScriptedModel::following([goal_completion_script()]),
        configuration.runtime_model_catalog(),
        None,
    );
    let execution = PostgresProviderModelExecution::new(
        PostgresModelCallRepository::new(
            pool.clone(),
            configuration.target_catalog(),
            ModelCallCredentialReference::new("goal-reload-fixture"),
        ),
        InProcessAttemptDispatchGate::default(),
        provider,
        None,
    )
    .with_tool_loop(
        InProcessToolDispatchGate::default(),
        NoToolCatalog,
        UnexpectedToolExecutor,
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        pool.clone(),
        None,
        Vec::new(),
    ));
    let mut activation = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = activation.execute(session).await? else {
        panic!("first goal turn activates")
    };
    execution.execute(activated).await?;
    source["aliases"]
        .as_array_of_tables_mut()
        .expect("aliases")
        .get_mut(0)
        .expect("alias")
        .insert("selection_id", toml_edit::value(next_selection.to_string()));
    std::fs::write(model_path, source.to_string())?;
    assert_eq!(
        reload
            .reload(ReloadConfiguration {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7())
            })
            .await?,
        ReloadLookup::Recorded(ReloadResult::Reloaded)
    );
    disposition.reconcile_success(session).await?;
    let selected: Uuid = sqlx::query_scalar("SELECT frozen_alias_selected_direct_id FROM queued_input_origin WHERE session_id = $1 AND turn_id <> $2")
        .bind(session.into_uuid()).bind(first.turn().into_uuid()).fetch_one(&pool).await?;
    assert_eq!(selected, next_selection);
    pool.close().await;
    Ok(())
}
