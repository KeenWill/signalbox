#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration tests use assertion panics and explicit fixture expectations"
)]

use std::{
    error::Error,
    fmt,
    sync::{Arc, Mutex},
};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    CreateSessionOutcome, CreateSessionRequest, CreateSessionService, DecideToolRequestService,
    InProcessAttemptDispatchGate, InProcessEligibilityWorkSource, InProcessToolDispatchGate,
    ModelCallCredentialReference, OperatorFailureClass, StartEligibleTurnOutcome,
    StartEligibleTurnService, StartupScanService, SubmitInputOutcome, SubmitInputRequest,
    SubmitInputService, ToolDefinition, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence, ToolInputSchema, UuidV7SessionIdGenerator,
    UuidV7StartEligibleTurnIdGenerator, UuidV7StartupScanIdGenerator, UuidV7SubmitInputIdGenerator,
    UuidV7ToolLoopIdGenerator,
};
use signalbox_domain::{
    ActivatedAcceptedInputTurn, DangerousToolAutoApproval, DecideToolRequest,
    DecideToolRequestResult, DeliveryRequest, DirectModelSelection, DurableCommandId, ModelCallId,
    ModelSelectionOverride, ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition,
    NormalizedToolArguments, PerInputConfigurationChoices, ProviderModelIdentity,
    ResolvedProviderTarget, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionId, SubmitInputAppliedResult, SubmitInputResult, ToolApprovalDecision, ToolEffectClass,
    ToolExecutionErrorDetail, ToolName, ToolPermissionDefault, ToolRequestId, TurnId, UserContent,
};
use signalbox_hubd::{
    ActivatedTurnExecution, PostgresContinuationToolLoopRepository, PostgresProviderModelExecution,
    PostgresProviderToolLoopExecution,
};
use signalbox_model_provider_runtime::{
    RuntimeModelCallProvider, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, ExchangeFacts, ProviderReportedModel,
    Script, ScriptedModel, TerminalEvidence, TokenUsage, ToolCallId,
    ToolCallProposal as RuntimeToolCallProposal, ToolName as RuntimeToolName,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository, local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository, scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository, startup::PostgresStartupScanRepository,
    submit_input::SubmitInputRepository, tool_loop::PostgresToolLoopRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_hubd_tool_loop_e2e";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

struct ToolLoopFixture {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    session: SessionId,
    turn: TurnId,
    activated: ActivatedAcceptedInputTurn,
    targets: ModelTargetCatalog,
    runtime_models: RuntimeModelCatalog,
    credential_reference: ModelCallCredentialReference,
    tool_dispatch_gate: InProcessToolDispatchGate,
}

impl ToolLoopFixture {
    async fn new(posture: DangerousToolAutoApproval, seed: u128) -> Result<Self, Box<dyn Error>> {
        let (container, pool) = migrated_postgres().await?;
        let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 1));
        let defaults = SessionConfigurationDefaults::with_dangerous_tool_auto_approval(
            ModelSelectionRequest::Direct(selection),
            posture,
        );
        let mut create = CreateSessionService::new(
            UuidV7SessionIdGenerator,
            CreateSessionRepository::new(pool.clone()),
        );
        let CreateSessionOutcome::Applied(created) = create
            .execute(CreateSessionRequest::try_new(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 2)),
                defaults,
            )?)
            .await?
        else {
            panic!("the unique fixture command must create its session")
        };
        let session = created.session();

        let sweep = PostgresEligibilitySweep::new(pool.clone());
        let (nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
        let tool_dispatch_gate = InProcessToolDispatchGate::default();
        let mut submit = SubmitInputService::new(
            UuidV7SubmitInputIdGenerator,
            SubmitInputRepository::new(pool.clone()),
            nudge,
        )
        .with_tool_dispatch_gate(tool_dispatch_gate.clone());
        let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(origin),
        )) = submit
            .execute(SubmitInputRequest::try_new(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 3)),
                session,
                UserContent::try_text(String::from("offline tool-loop request"))
                    .expect("fixture user content is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: default_configuration(),
                },
            )?)
            .await?
        else {
            panic!("the unique fixture input must create queued origin work")
        };
        let turn = origin.turn();

        let mut start = StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(pool.clone()),
        );
        let StartEligibleTurnOutcome::Activated(activated) = start.execute(session).await? else {
            panic!("the queued fixture turn must activate")
        };

        let provider_identity = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 4));
        let target = ResolvedProviderTarget::naming(provider_identity);
        let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
            selection, target,
        )])
        .expect("one fixture target definition is unique");
        let runtime_models =
            RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
                target,
                String::from("scripted-tool-loop"),
                64,
            )
            .expect("fixture runtime definition is valid")])
            .expect("one fixture runtime target is unique");

        Ok(Self {
            _container: container,
            pool,
            session,
            turn,
            activated: *activated,
            targets,
            runtime_models,
            credential_reference: ModelCallCredentialReference::new("scripted-tool-loop-test"),
            tool_dispatch_gate,
        })
    }

    fn execution<Catalog, Executor>(
        &self,
        scripts: impl IntoIterator<Item = Script>,
        catalog: Catalog,
        executor: Executor,
    ) -> PostgresProviderToolLoopExecution<
        RuntimeModelCallProvider<ScriptedModel<ModelCallId>>,
        Catalog,
        Executor,
    > {
        let provider = RuntimeModelCallProvider::new(
            ScriptedModel::<ModelCallId>::following(scripts),
            self.runtime_models.clone(),
        );
        PostgresProviderModelExecution::new(
            PostgresModelCallRepository::new(
                self.pool.clone(),
                self.targets.clone(),
                self.credential_reference.clone(),
            ),
            InProcessAttemptDispatchGate::default(),
            provider,
        )
        .with_tool_loop(
            PostgresContinuationToolLoopRepository::new(
                self.pool.clone(),
                self.targets.clone(),
                self.credential_reference.clone(),
            ),
            self.tool_dispatch_gate.clone(),
            catalog,
            executor,
        )
    }

    async fn request_ids(&self) -> Result<Vec<ToolRequestId>, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT request_id
               FROM tool_request
              WHERE session_id = $1
                AND turn_id = $2
              ORDER BY request_ordinal",
        )
        .bind(self.session.into_uuid())
        .bind(self.turn.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map(|ids| ids.into_iter().map(ToolRequestId::from_uuid).collect())
    }

    async fn decide(
        &self,
        request: ToolRequestId,
        decision: ToolApprovalDecision,
        command: u128,
    ) -> Result<(), Box<dyn Error>> {
        let mut service = DecideToolRequestService::new(
            UuidV7ToolLoopIdGenerator,
            PostgresToolLoopRepository::new(self.pool.clone()),
        );
        let prepared = service
            .execute(DecideToolRequest::new(
                DurableCommandId::from_uuid(Uuid::from_u128(command)),
                request,
                decision,
            ))
            .await?;
        assert!(
            matches!(prepared.result(), DecideToolRequestResult::Applied(_)),
            "the earliest undecided request must accept its owner decision"
        );
        Ok(())
    }

    async fn transcript_kinds(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT entry.payload_kind
               FROM turn_lifecycle AS lifecycle
               JOIN context_frontier_member AS member
                 ON member.owning_session_id = lifecycle.session_id
                AND member.context_frontier_id = lifecycle.terminal_frontier_id
               JOIN semantic_transcript_entry AS entry
                 ON entry.source_session_id = member.source_session_id
                AND entry.semantic_entry_id = member.semantic_entry_id
              WHERE lifecycle.session_id = $1
                AND lifecycle.turn_id = $2
              ORDER BY member.member_position",
        )
        .bind(self.session.into_uuid())
        .bind(self.turn.into_uuid())
        .fetch_all(&self.pool)
        .await
    }
}

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
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
    Ok((container, pool))
}

const fn default_configuration() -> PerInputConfigurationChoices {
    PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::first(),
        ModelSelectionOverride::UseSessionDefault,
    )
}

fn tool_name(value: &str) -> ToolName {
    ToolName::try_new(value.to_owned()).expect("fixture tool name is valid")
}

fn tool(name: &str, permission: ToolPermissionDefault, effect: ToolEffectClass) -> CompiledTool {
    let definition = ToolDefinition::new(
        tool_name(name),
        format!("Runs the {name} fixture tool."),
        ToolInputSchema::try_new(String::from(
            r#"{"additionalProperties":true,"type":"object"}"#,
        ))
        .expect("fixture schema is valid"),
        permission,
        effect,
    );
    CompiledTool::new(definition, |_arguments: &NormalizedToolArguments| {
        Ok::<(), ToolExecutionErrorDetail>(())
    })
}

fn catalog(tools: impl IntoIterator<Item = CompiledTool>) -> CompiledToolCatalog {
    CompiledToolCatalog::try_new(tools).expect("fixture tool declarations are unique")
}

fn tool_use_script(calls: &[(&str, &str)]) -> Script {
    let content = calls
        .iter()
        .enumerate()
        .map(|(ordinal, (name, arguments))| {
            AssistantPart::ToolCall(RuntimeToolCallProposal {
                id: ToolCallId::new(format!("fixture-call-{ordinal}")),
                name: RuntimeToolName::new(*name),
                arguments_json: (*arguments).to_owned(),
            })
        })
        .collect();
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new("scripted-tool-loop")),
        finish: CompletionFinish::ToolUse,
        content,
        usage: TokenUsage::unreported(),
    }))
}

fn completion_script(text: &str) -> Script {
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new("scripted-tool-loop")),
        finish: CompletionFinish::EndTurn,
        content: vec![AssistantPart::Text(text.to_owned())],
        usage: TokenUsage::unreported(),
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExecutorMode {
    Complete,
    LoseProcess,
}

#[derive(Clone, Debug)]
struct RecordingExecutor {
    mode: ExecutorMode,
    events: Arc<Mutex<Vec<String>>>,
    arguments: Arc<Mutex<Vec<String>>>,
}

impl RecordingExecutor {
    fn completing() -> Self {
        Self {
            mode: ExecutorMode::Complete,
            events: Arc::new(Mutex::new(Vec::new())),
            arguments: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn losing_process() -> Self {
        Self {
            mode: ExecutorMode::LoseProcess,
            events: Arc::new(Mutex::new(Vec::new())),
            arguments: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn events(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("fixture event lock is available")
            .clone()
    }

    fn arguments(&self) -> Vec<String> {
        self.arguments
            .lock()
            .expect("fixture argument lock is available")
            .clone()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FixtureExecutorError;

impl fmt::Display for FixtureExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("fixture executor lost its process after dispatch")
    }
}

impl Error for FixtureExecutorError {}

impl ClassifyOperatorFailure for FixtureExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: true,
        }
    }
}

impl ToolExecutor for RecordingExecutor {
    type Error = FixtureExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        self.events
            .lock()
            .expect("fixture event lock is available")
            .push(invocation.request().name().as_str().to_owned());
        self.arguments
            .lock()
            .expect("fixture argument lock is available")
            .push(invocation.request().arguments().as_str().to_owned());
        let name = invocation.request().name().as_str().to_owned();
        match self.mode {
            ExecutorMode::Complete => Ok(invocation.bind(ToolExecutorEvidence::CompletedText(
                format!("completed:{name}"),
            ))),
            ExecutorMode::LoseProcess => Err(FixtureExecutorError),
        }
    }
}

/// S10 / S11 / INV-004 / INV-005 / INV-019 / INV-021 / INV-024:
/// one offline scripted turn parks for an owner decision, executes exactly
/// once after approval, commits a reference-only result at the continuation
/// boundary, and completes only after the second model round.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_s11_tool_loop_parks_approves_executes_continues_and_completes()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled, 0x3100).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let execution = fixture.execution(
        [
            tool_use_script(&[("confirmed", r#"{"value":"one"}"#)]),
            completion_script("tool result observed"),
        ],
        tool_catalog,
        executor.clone(),
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let requests = fixture.request_ids().await?;
    assert_eq!(requests.len(), 1);
    let parked: (String, Option<Uuid>, i64) = sqlx::query_as(
        "SELECT active_phase_kind, approval_tool_request_id,
                (SELECT count(*) FROM tool_attempt WHERE turn_id = $2)
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        parked,
        (
            String::from("awaiting_tool_approval"),
            Some(requests[0].into_uuid()),
            0,
        )
    );

    fixture
        .decide(requests[0], ToolApprovalDecision::Approve, 0x3110)
        .await?;
    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;

    assert_eq!(executor.events(), vec![String::from("confirmed")]);
    assert_eq!(
        executor.arguments(),
        vec![String::from(r#"{"value":"one"}"#)]
    );
    assert_eq!(
        fixture.transcript_kinds().await?,
        vec![
            "origin_accepted_input",
            "assistant_tool_use",
            "tool_execution_result",
            "assistant_text",
            "turn_completed",
        ]
    );
    let terminal_shape: (String, String, String, i64) = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind,
                attempt.terminal_disposition_kind,
                attempt.result_text,
                (SELECT count(*) FROM model_call
                  WHERE session_id = $1 AND turn_id = $2)
           FROM turn_lifecycle AS lifecycle
           JOIN tool_attempt AS attempt
             ON attempt.session_id = lifecycle.session_id
            AND attempt.turn_id = lifecycle.turn_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        terminal_shape,
        (
            String::from("completed"),
            String::from("completed"),
            String::from("completed:confirmed"),
            2,
        )
    );
    Ok(())
}

/// S10 / S11 / INV-019 / INV-020 / INV-027: owner denial creates no physical
/// attempt, projects one error result to the continuation call, and allows the
/// same turn to complete from the model's response.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_s11_inv020_inv027_denial_continues_without_execution() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled, 0x3200).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let execution = fixture.execution(
        [
            tool_use_script(&[("confirmed", "{}")]),
            completion_script("denial observed"),
        ],
        tool_catalog,
        executor.clone(),
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.request_ids().await?[0];
    fixture
        .decide(request, ToolApprovalDecision::Deny { reason: None }, 0x3210)
        .await?;
    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;

    assert!(executor.events().is_empty());
    assert_eq!(
        fixture.transcript_kinds().await?,
        vec![
            "origin_accepted_input",
            "assistant_tool_use",
            "tool_denied",
            "assistant_text",
            "turn_completed",
        ]
    );
    let denial_shape: (String, String, i64) = sqlx::query_as(
        "SELECT decision_kind, decision_source,
                (SELECT count(*) FROM tool_attempt WHERE request_id = $1)
           FROM tool_approval_decision
          WHERE request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        denial_shape,
        (String::from("deny"), String::from("owner_command"), 0)
    );
    Ok(())
}

/// S10 / INV-019 / INV-020 / INV-027 / INV-029 / INV-037: deny-and-end first
/// records the exact denial, then the ordinary proof-bearing interrupt closes
/// the active turn; no tool attempt is created and the stop remains
/// independently auditable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv020_inv027_inv029_inv037_denial_composes_with_interrupt_to_end_turn()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled, 0x3300).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let execution = fixture.execution(
        [tool_use_script(&[("confirmed", "{}")])],
        tool_catalog,
        executor.clone(),
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.request_ids().await?[0];
    fixture
        .decide(request, ToolApprovalDecision::Deny { reason: None }, 0x3310)
        .await?;

    let sweep = PostgresEligibilitySweep::new(fixture.pool.clone());
    let (nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let mut submit = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        SubmitInputRepository::new(fixture.pool.clone()),
        nudge,
    )
    .with_tool_dispatch_gate(fixture.tool_dispatch_gate.clone());
    let interrupt_command = DurableCommandId::from_uuid(Uuid::from_u128(0x3311));
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit
        .execute(SubmitInputRequest::try_new(
            interrupt_command,
            fixture.session,
            UserContent::try_text(String::from("stop after denying"))
                .expect("fixture interrupt content is admitted"),
            DeliveryRequest::Interrupt {
                expected_active_turn: fixture.turn,
                configuration: default_configuration(),
            },
        )?)
        .await?
    else {
        panic!("the exact active turn must accept its interrupt")
    };
    let applied_interrupt = origin
        .applied_interrupt()
        .expect("the interrupt origin must retain its proof");
    assert_eq!(origin.turn(), applied_interrupt.successor());
    assert_eq!(origin.accepted_input(), applied_interrupt.accepted_input());
    assert_eq!(origin.queue_order(), applied_interrupt.successor_order());
    assert_eq!(applied_interrupt.proof().command(), interrupt_command);
    assert_eq!(applied_interrupt.proof().predecessor(), fixture.turn);

    assert!(executor.events().is_empty());
    let terminal_shape: (String, i64, i64, i64) = sqlx::query_as(
        "SELECT terminal_disposition_kind,
                (SELECT count(*) FROM tool_attempt WHERE request_id = $3),
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE source_session_id = $1
                    AND payload_kind = 'tool_denied'
                    AND tool_result_request_id = $3),
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE source_session_id = $1
                    AND payload_kind = 'turn_cancelled'
                    AND cancelled_turn_id = $2)
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(request.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(terminal_shape, (String::from("cancelled"), 0, 1, 1));
    Ok(())
}

/// S02 / S10 / INV-005 / INV-006 / INV-019: a restart scan preserves an
/// approval wait exactly; a fresh composition can later consume the owner's
/// approval and continue the same logical turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s10_inv005_inv006_restart_leaves_approval_turn_parked() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled, 0x3400).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    fixture
        .execution(
            [tool_use_script(&[("confirmed", "{}")])],
            tool_catalog.clone(),
            executor.clone(),
        )
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.request_ids().await?[0];

    let mut scan = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(fixture.pool.clone()),
    );
    let outcome = scan.execute().await?;
    assert!(outcome.is_complete());
    assert_eq!(outcome.recovered_turn_count(), 0);
    let parked: (String, Option<Uuid>, i64) = sqlx::query_as(
        "SELECT active_phase_kind, approval_tool_request_id,
                (SELECT count(*) FROM tool_attempt WHERE request_id = $3)
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(request.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        parked,
        (
            String::from("awaiting_tool_approval"),
            Some(request.into_uuid()),
            0,
        )
    );

    fixture
        .decide(request, ToolApprovalDecision::Approve, 0x3410)
        .await?;
    fixture
        .execution(
            [completion_script("continued after restart")],
            tool_catalog,
            executor.clone(),
        )
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    assert_eq!(executor.events(), vec![String::from("confirmed")]);
    Ok(())
}

/// S10 / S11 / INV-019 / INV-020 / INV-021: an auto/confirm batch parks on
/// its earliest undecided request and, after approval, executes both requests
/// serially in proposal order with their distinct provenance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_s11_inv019_inv020_inv021_mixed_batch_executes_in_proposal_order()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled, 0x3500).await?;
    let tool_catalog = catalog([
        tool(
            "automatic",
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        ),
        tool(
            "confirmed",
            ToolPermissionDefault::Confirm,
            ToolEffectClass::EffectFree,
        ),
    ]);
    let executor = RecordingExecutor::completing();
    let execution = fixture.execution(
        [
            tool_use_script(&[("automatic", "{}"), ("confirmed", "{}")]),
            completion_script("batch observed"),
        ],
        tool_catalog,
        executor.clone(),
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let requests = fixture.request_ids().await?;
    assert_eq!(requests.len(), 2);
    fixture
        .decide(requests[1], ToolApprovalDecision::Approve, 0x3510)
        .await?;
    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;

    assert_eq!(
        executor.events(),
        vec![String::from("automatic"), String::from("confirmed")]
    );
    let ordered: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT request.tool_name, approval.decision_source,
                attempt.terminal_disposition_kind
           FROM tool_request AS request
           JOIN tool_approval_decision AS approval
             ON approval.request_id = request.request_id
           JOIN tool_attempt AS attempt
             ON attempt.request_id = request.request_id
          WHERE request.session_id = $1
            AND request.turn_id = $2
          ORDER BY request.request_ordinal",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    assert_eq!(
        ordered,
        vec![
            (
                String::from("automatic"),
                String::from("policy_auto"),
                String::from("completed"),
            ),
            (
                String::from("confirmed"),
                String::from("owner_command"),
                String::from("completed"),
            ),
        ]
    );
    Ok(())
}

/// S10 / INV-020 / INV-021: the explicitly dangerous frozen blanket posture
/// approves a confirm-default tool under `session_blanket` provenance and the
/// turn runs unattended without fabricating owner agency.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv020_inv021_blanket_posture_runs_confirm_tool_unattended()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::ApproveAll, 0x3600).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    fixture
        .execution(
            [
                tool_use_script(&[("confirmed", "{}")]),
                completion_script("blanket result observed"),
            ],
            tool_catalog,
            executor.clone(),
        )
        .execute(Box::new(fixture.activated.clone()))
        .await?;

    assert_eq!(executor.events(), vec![String::from("confirmed")]);
    let approval: (String, Option<Uuid>, String) = sqlx::query_as(
        "SELECT approval.decision_source, approval.owner_command_id,
                lifecycle.terminal_disposition_kind
           FROM tool_approval_decision AS approval
           JOIN tool_request AS request
             ON request.request_id = approval.request_id
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.session_id = request.session_id
            AND lifecycle.turn_id = request.turn_id
          WHERE request.session_id = $1
            AND request.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        approval,
        (
            String::from("session_blanket"),
            None,
            String::from("completed")
        )
    );
    Ok(())
}

/// S10 / INV-005 / INV-024: losing a dispatched effect-free attempt
/// never retries it; a fresh process classifies it `known_failed` with
/// `crash_lost` evidence and fails the turn honestly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv005_inv024_effect_free_crash_is_known_failed_without_retry()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled, 0x3700).await?;
    let tool_catalog = catalog([tool(
        "effect_free",
        ToolPermissionDefault::Auto,
        ToolEffectClass::EffectFree,
    )]);
    let crashing = RecordingExecutor::losing_process();
    let first = fixture
        .execution(
            [tool_use_script(&[("effect_free", "{}")])],
            tool_catalog.clone(),
            crashing.clone(),
        )
        .execute(Box::new(fixture.activated.clone()))
        .await;
    assert!(
        first.is_err(),
        "fixture process loss must escape orchestration"
    );
    assert_eq!(crashing.events(), vec![String::from("effect_free")]);

    let resumed = RecordingExecutor::completing();
    fixture
        .execution([], tool_catalog, resumed.clone())
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    assert!(
        resumed.events().is_empty(),
        "crash recovery must not redispatch"
    );
    let classified: (String, String, String) = sqlx::query_as(
        "SELECT attempt.terminal_disposition_kind, attempt.error_kind,
                lifecycle.terminal_disposition_kind
           FROM tool_attempt AS attempt
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.session_id = attempt.session_id
            AND lifecycle.turn_id = attempt.turn_id
          WHERE attempt.session_id = $1
            AND attempt.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        classified,
        (
            String::from("known_failed"),
            String::from("crash_lost"),
            String::from("failed"),
        )
    );
    Ok(())
}

/// S10 / INV-005 / INV-024: losing a dispatched external-effect
/// attempt never retries it; a fresh process classifies exact ambiguity and
/// parks the turn for owner recovery.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv005_inv024_external_effect_crash_parks_ambiguous_without_retry()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled, 0x3800).await?;
    let tool_catalog = catalog([tool(
        "external_effect",
        ToolPermissionDefault::Auto,
        ToolEffectClass::ExternalEffect,
    )]);
    let crashing = RecordingExecutor::losing_process();
    let first = fixture
        .execution(
            [tool_use_script(&[("external_effect", "{}")])],
            tool_catalog.clone(),
            crashing.clone(),
        )
        .execute(Box::new(fixture.activated.clone()))
        .await;
    assert!(
        first.is_err(),
        "fixture process loss must escape orchestration"
    );
    assert_eq!(crashing.events(), vec![String::from("external_effect")]);

    let resumed = RecordingExecutor::completing();
    fixture
        .execution([], tool_catalog, resumed.clone())
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    assert!(
        resumed.events().is_empty(),
        "crash recovery must not redispatch"
    );
    let classified: (String, String, Option<Uuid>) = sqlx::query_as(
        "SELECT attempt.terminal_disposition_kind,
                lifecycle.active_phase_kind,
                lifecycle.recovery_tool_attempt_id
           FROM tool_attempt AS attempt
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.session_id = attempt.session_id
            AND lifecycle.turn_id = attempt.turn_id
          WHERE attempt.session_id = $1
            AND attempt.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let attempt: Uuid = sqlx::query_scalar(
        "SELECT attempt_id FROM tool_attempt WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        classified,
        (
            String::from("ambiguous"),
            String::from("awaiting_tool_recovery"),
            Some(attempt),
        )
    );
    Ok(())
}
