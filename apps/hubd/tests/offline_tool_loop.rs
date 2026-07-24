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
    ActivatedTurnExecution, PostgresProviderModelExecution, PostgresProviderToolLoopExecution,
};
use signalbox_model_provider_runtime::{
    RuntimeModelCallProvider, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionEvidence, CompletionFinish, ExchangeFacts,
    MessagePart, ModelOperation, ModelRuntime, ObservationSink, PreparationOutcome,
    ProviderReportedModel, Script, ScriptedModel, ScriptedPrepared, TerminalEvidence,
    TerminalReport, TokenUsage, ToolCallId, ToolCallProposal as RuntimeToolCallProposal,
    ToolName as RuntimeToolName, ToolResultRecord,
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
const FIXTURE_ID_SEED: u128 = 0x3100;
const DECISION_COMMAND_ID: u128 = 0x3110;

#[derive(Clone, Debug)]
struct RecordingScriptedModel {
    inner: Arc<ScriptedModel<ModelCallId>>,
}

impl ModelRuntime<ModelCallId> for RecordingScriptedModel {
    type Prepared = ScriptedPrepared<ModelCallId>;

    async fn prepare(
        &self,
        operation: ModelOperation<ModelCallId>,
        cancellation: CancellationSignal,
    ) -> PreparationOutcome<ModelCallId, Self::Prepared> {
        self.inner.prepare(operation, cancellation).await
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<ModelCallId> + Send),
        cancellation: CancellationSignal,
    ) -> TerminalReport<ModelCallId> {
        self.inner.execute(prepared, sink, cancellation).await
    }
}

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
    async fn new(posture: DangerousToolAutoApproval) -> Result<Self, Box<dyn Error>> {
        let (container, pool) = migrated_postgres().await?;
        let selection = DirectModelSelection::from_uuid(Uuid::from_u128(FIXTURE_ID_SEED + 1));
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
                DurableCommandId::from_uuid(Uuid::from_u128(FIXTURE_ID_SEED + 2)),
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
            tool_dispatch_gate.clone(),
        );
        let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(origin),
        )) = submit
            .execute(SubmitInputRequest::try_new(
                DurableCommandId::from_uuid(Uuid::from_u128(FIXTURE_ID_SEED + 3)),
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

        let provider_identity =
            ProviderModelIdentity::from_uuid(Uuid::from_u128(FIXTURE_ID_SEED + 4));
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
    ) -> (
        PostgresProviderToolLoopExecution<
            RuntimeModelCallProvider<RecordingScriptedModel>,
            Catalog,
            Executor,
        >,
        Arc<ScriptedModel<ModelCallId>>,
    ) {
        let runtime = Arc::new(ScriptedModel::<ModelCallId>::following(scripts));
        let provider = RuntimeModelCallProvider::new(
            RecordingScriptedModel {
                inner: Arc::clone(&runtime),
            },
            self.runtime_models.clone(),
        );
        (
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    self.pool.clone(),
                    self.targets.clone(),
                    self.credential_reference.clone(),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
            )
            .with_tool_loop(self.tool_dispatch_gate.clone(), catalog, executor),
            runtime,
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
    ) -> Result<(), Box<dyn Error>> {
        let mut service = DecideToolRequestService::new(
            UuidV7ToolLoopIdGenerator,
            PostgresToolLoopRepository::new(self.pool.clone()),
        );
        let prepared = service
            .execute(DecideToolRequest::new(
                DurableCommandId::from_uuid(Uuid::from_u128(DECISION_COMMAND_ID)),
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

    async fn wait_for_requests(
        &self,
        expected: usize,
    ) -> Result<Vec<ToolRequestId>, Box<dyn Error>> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let requests = self.request_ids().await?;
                if requests.len() == expected {
                    return Ok::<_, sqlx::Error>(requests);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| std::io::Error::other("tool requests were not durably parked"))?
        .map_err(Into::into)
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

fn continuation_tool_results(
    runtime: &ScriptedModel<ModelCallId>,
) -> Result<Vec<ToolResultRecord>, Box<dyn Error>> {
    let operations = runtime.received_operations();
    let continuation = operations
        .get(1)
        .ok_or_else(|| std::io::Error::other("continuation model operation was not received"))?;
    Ok(continuation
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            MessagePart::ToolResult(result) => Some(result.clone()),
            MessagePart::Text(_)
            | MessagePart::ToolCall(_)
            | MessagePart::Thinking { .. }
            | MessagePart::RedactedThinking { .. } => None,
        })
        .collect())
}

#[track_caller]
fn assert_confirmed_catalog(operation: &ModelOperation<ModelCallId>) {
    let [definition] = operation.tools.as_slice() else {
        panic!("each model operation carries the one compiled definition")
    };
    assert_eq!(definition.name.as_str(), "confirmed");
    assert_eq!(definition.description, "Runs the confirmed fixture tool.");
    assert_eq!(
        definition.input_schema.get(),
        r#"{"additionalProperties":true,"type":"object"}"#
    );
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

#[derive(Clone, Debug)]
struct SerialProbeExecutor {
    events: Arc<Mutex<Vec<String>>>,
    first_entered: Arc<tokio::sync::Notify>,
    release_first: Arc<tokio::sync::Notify>,
}

impl SerialProbeExecutor {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            first_entered: Arc::new(tokio::sync::Notify::new()),
            release_first: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn events(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("fixture event lock is available")
            .clone()
    }

    async fn wait_for_first(&self) -> Result<(), tokio::time::error::Elapsed> {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.first_entered.notified(),
        )
        .await
    }

    fn release_first(&self) {
        self.release_first.notify_one();
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

impl ToolExecutor for SerialProbeExecutor {
    type Error = FixtureExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let name = invocation.request().name().as_str().to_owned();
        let is_first = {
            let mut events = self.events.lock().expect("fixture event lock is available");
            events.push(name.clone());
            events.len() == 1
        };
        if is_first {
            self.first_entered.notify_one();
            self.release_first.notified().await;
        }
        Ok(invocation.bind(ToolExecutorEvidence::CompletedText(format!(
            "completed:{name}"
        ))))
    }
}

/// S10 / INV-004 / INV-005 / INV-019 / INV-021 / INV-024:
/// one offline scripted turn parks for an owner decision, executes exactly
/// once after approval with normalized arguments, commits a reference-only
/// result at the continuation boundary, and completes only after the second
/// model round.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv004_inv005_inv019_inv021_inv024_tool_loop_completes() -> Result<(), Box<dyn Error>>
{
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[("confirmed", r#"{ "value" : "one" }"#)]),
            completion_script("tool result observed"),
        ],
        tool_catalog,
        executor.clone(),
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let requests = fixture.wait_for_requests(1).await?;
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
        .decide(requests[0], ToolApprovalDecision::Approve)
        .await?;
    execution.resume_active(fixture.session).await?;

    assert_eq!(executor.events(), vec![String::from("confirmed")]);
    assert_eq!(
        executor.arguments(),
        vec![String::from(r#"{"value":"one"}"#)]
    );
    assert_eq!(
        continuation_tool_results(&runtime)?,
        vec![ToolResultRecord {
            tool_call_id: ToolCallId::new(requests[0].into_uuid().to_string()),
            content: String::from("completed:confirmed"),
            is_error: false,
        }]
    );
    let operations = runtime.received_operations();
    let [initial, continuation] = operations.as_slice() else {
        panic!("the completed tool loop has exactly two model operations")
    };
    assert_confirmed_catalog(initial);
    assert_confirmed_catalog(continuation);
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
    let identity_shape: (Uuid, Uuid, Uuid, Uuid, Uuid, i64) = sqlx::query_as(
        "SELECT request.request_id,
                attempt.attempt_id,
                attempt.request_id,
                attempt.turn_id,
                attempt.issuing_turn_attempt_id,
                attempt.dispatch_generation::bigint
           FROM tool_request AS request
           JOIN tool_attempt AS attempt
             ON attempt.request_id = request.request_id
            AND attempt.turn_id = request.turn_id
            AND attempt.session_id = request.session_id
          WHERE request.session_id = $1
            AND request.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(identity_shape.0, requests[0].into_uuid());
    assert_ne!(identity_shape.0, identity_shape.1);
    assert_eq!(identity_shape.2, identity_shape.0);
    assert_eq!(identity_shape.3, fixture.turn.into_uuid());
    assert_ne!(identity_shape.4, identity_shape.0);
    assert_ne!(identity_shape.4, identity_shape.1);
    assert_eq!(identity_shape.5, 1);
    Ok(())
}

/// S10 / S11 / INV-019 / INV-020 / INV-027: owner denial creates no physical
/// attempt, projects one error result to the continuation call, and allows the
/// same turn to complete from the model's response.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_s11_inv020_inv027_denial_continues_without_execution() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let (execution, runtime) = fixture.execution(
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
    let request = fixture.wait_for_requests(1).await?[0];
    fixture
        .decide(request, ToolApprovalDecision::Deny { reason: None })
        .await?;
    execution.resume_active(fixture.session).await?;

    assert!(executor.events().is_empty());
    assert_eq!(
        continuation_tool_results(&runtime)?,
        vec![ToolResultRecord {
            tool_call_id: ToolCallId::new(request.into_uuid().to_string()),
            content: serde_json::json!({
                "error": {
                    "kind": "denied",
                    "detail": null,
                }
            })
            .to_string(),
            is_error: true,
        }]
    );
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
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let (execution, _runtime) = fixture.execution(
        [tool_use_script(&[("confirmed", "{}")])],
        tool_catalog,
        executor.clone(),
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.wait_for_requests(1).await?[0];
    fixture
        .decide(request, ToolApprovalDecision::Deny { reason: None })
        .await?;

    let sweep = PostgresEligibilitySweep::new(fixture.pool.clone());
    let (nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let mut submit = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        SubmitInputRepository::new(fixture.pool.clone()),
        nudge,
        fixture.tool_dispatch_gate.clone(),
    );
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
    assert_eq!(
        fixture.transcript_kinds().await?,
        vec![
            "origin_accepted_input",
            "assistant_tool_use",
            "tool_denied",
            "turn_cancelled",
        ],
        "the denial result must precede the independently authorized cancellation marker"
    );
    Ok(())
}

/// S02 / S10 / INV-005 / INV-006 / INV-019: a restart scan preserves an
/// approval wait exactly; after the owner decision, the durable sweep and a
/// fresh composition resume the same logical turn without replaying activation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s10_inv005_inv006_restart_leaves_approval_turn_parked() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let (first_execution, _first_runtime) = fixture.execution(
        [tool_use_script(&[("confirmed", "{}")])],
        tool_catalog.clone(),
        executor.clone(),
    );
    first_execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.wait_for_requests(1).await?[0];

    let mut scan = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(fixture.pool.clone()),
    );
    let outcome = scan.execute().await?;
    assert!(outcome.is_complete());
    assert_eq!(outcome.recovered_turn_count(), 0);
    let repeated = scan.execute().await?;
    assert!(repeated.is_complete());
    assert_eq!(repeated.recovered_turn_count(), 0);
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
        .decide(request, ToolApprovalDecision::Approve)
        .await?;
    let (resumable, continuation) = PostgresEligibilitySweep::new(fixture.pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(!continuation);
    assert_eq!(resumable, vec![fixture.session]);
    let (restarted_execution, _restarted_runtime) = fixture.execution(
        [completion_script("continued after restart")],
        tool_catalog,
        executor.clone(),
    );
    restarted_execution.resume_active(fixture.session).await?;
    assert_eq!(executor.events(), vec![String::from("confirmed")]);
    Ok(())
}

/// S10 / INV-019 / INV-020 / INV-021: an auto/confirm batch parks on
/// its earliest undecided request and, after approval, executes both requests
/// serially in proposal order with their distinct provenance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv019_inv020_inv021_mixed_batch_executes_in_proposal_order()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
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
    let executor = SerialProbeExecutor::new();
    let (execution, _runtime) = fixture.execution(
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
    let requests = fixture.wait_for_requests(2).await?;
    assert_eq!(requests.len(), 2);
    assert!(
        executor.events().is_empty(),
        "the batch barrier keeps auto-approved work undispatched"
    );
    let parked_request: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT active_phase_kind, approval_tool_request_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        parked_request,
        (
            String::from("awaiting_tool_approval"),
            Some(requests[1].into_uuid()),
        )
    );
    fixture
        .decide(requests[1], ToolApprovalDecision::Approve)
        .await?;
    let (resumed, observed_first) = tokio::join!(execution.resume_active(fixture.session), async {
        executor.wait_for_first().await?;
        assert_eq!(
            executor.events(),
            vec![String::from("automatic")],
            "the second executor must not enter while the first remains pending"
        );
        executor.release_first();
        Ok::<(), tokio::time::error::Elapsed>(())
    });
    observed_first?;
    resumed?;

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
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::ApproveAll).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let (execution, _runtime) = fixture.execution(
        [
            tool_use_script(&[("confirmed", "{}")]),
            completion_script("blanket result observed"),
        ],
        tool_catalog,
        executor.clone(),
    );
    execution
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

/// S05 / INV-005 / INV-006 / INV-024: losing a dispatched effect-free attempt
/// never retries it; startup idempotently classifies it `known_failed` with
/// `crash_lost` evidence, closes the request, and then fails the turn honestly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s05_inv005_inv006_inv024_effect_free_crash_is_known_failed_without_retry()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "effect_free",
        ToolPermissionDefault::Auto,
        ToolEffectClass::EffectFree,
    )]);
    let crashing = RecordingExecutor::losing_process();
    let (first_execution, _runtime) = fixture.execution(
        [tool_use_script(&[("effect_free", "{}")])],
        tool_catalog,
        crashing.clone(),
    );
    let first = first_execution
        .execute(Box::new(fixture.activated.clone()))
        .await;
    assert!(
        first.is_err(),
        "fixture process loss must escape orchestration"
    );
    assert_eq!(crashing.events(), vec![String::from("effect_free")]);

    let mut startup = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(fixture.pool.clone()),
    );
    let recovery = startup.execute().await?;
    assert!(recovery.is_complete());
    assert_eq!(recovery.recovered_turn_count(), 1);
    let repeated_recovery = startup.execute().await?;
    assert!(repeated_recovery.is_complete());
    assert_eq!(repeated_recovery.recovered_turn_count(), 0);
    assert_eq!(crashing.events(), vec![String::from("effect_free")]);
    let classified: (String, String, String, String) = sqlx::query_as(
        "SELECT attempt.terminal_disposition_kind, attempt.error_kind,
                lifecycle.terminal_disposition_kind,
                issuing.end_disposition
           FROM tool_attempt AS attempt
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.session_id = attempt.session_id
            AND lifecycle.turn_id = attempt.turn_id
           JOIN turn_attempt AS issuing
             ON issuing.turn_attempt_id = attempt.issuing_turn_attempt_id
            AND issuing.turn_id = attempt.turn_id
            AND issuing.session_id = attempt.session_id
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
            String::from("lost"),
        )
    );
    assert_eq!(
        fixture.transcript_kinds().await?,
        vec![
            "origin_accepted_input",
            "assistant_tool_use",
            "tool_closed_by_turn_end",
            "turn_failed",
        ]
    );
    Ok(())
}

/// S06 / INV-005 / INV-024: losing a dispatched external-effect
/// attempt never retries it; startup idempotently classifies exact ambiguity
/// without projecting a result or close, and parks the turn for owner recovery.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s06_inv005_inv024_external_effect_crash_parks_ambiguous_without_retry()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "external_effect",
        ToolPermissionDefault::Auto,
        ToolEffectClass::ExternalEffect,
    )]);
    let crashing = RecordingExecutor::losing_process();
    let (first_execution, _runtime) = fixture.execution(
        [tool_use_script(&[("external_effect", "{}")])],
        tool_catalog,
        crashing.clone(),
    );
    let first = first_execution
        .execute(Box::new(fixture.activated.clone()))
        .await;
    assert!(
        first.is_err(),
        "fixture process loss must escape orchestration"
    );
    assert_eq!(crashing.events(), vec![String::from("external_effect")]);

    let mut startup = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(fixture.pool.clone()),
    );
    let recovery = startup.execute().await?;
    assert!(recovery.is_complete());
    assert_eq!(
        recovery.recovered_turn_count(),
        0,
        "the ambiguous turn remains active while its attempt is recovered"
    );
    let repeated_recovery = startup.execute().await?;
    assert!(repeated_recovery.is_complete());
    assert_eq!(
        repeated_recovery.recovered_turn_count(),
        0,
        "repeated startup leaves the same ambiguous turn parked"
    );
    assert_eq!(crashing.events(), vec![String::from("external_effect")]);
    let classified: (String, String, Option<Uuid>, String) = sqlx::query_as(
        "SELECT attempt.terminal_disposition_kind,
                lifecycle.active_phase_kind,
                lifecycle.recovery_tool_attempt_id,
                issuing.end_disposition
           FROM tool_attempt AS attempt
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.session_id = attempt.session_id
            AND lifecycle.turn_id = attempt.turn_id
           JOIN turn_attempt AS issuing
             ON issuing.turn_attempt_id = attempt.issuing_turn_attempt_id
            AND issuing.turn_id = attempt.turn_id
            AND issuing.session_id = attempt.session_id
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
            String::from("lost"),
        )
    );
    let semantic_tool_kinds: Vec<String> = sqlx::query_scalar(
        "SELECT entry.payload_kind
           FROM semantic_transcript_entry AS entry
          WHERE entry.source_session_id = $1
            AND (
                EXISTS (
                    SELECT 1
                      FROM model_call
                     WHERE model_call.model_call_id =
                           entry.producing_model_call_id
                       AND model_call.turn_id = $2
                )
                OR EXISTS (
                    SELECT 1
                      FROM tool_request
                     WHERE tool_request.turn_id = $2
                       AND tool_request.request_id IN (
                           entry.assistant_tool_request_id,
                           entry.tool_result_request_id
                       )
                )
                OR EXISTS (
                    SELECT 1
                      FROM tool_attempt
                     WHERE tool_attempt.turn_id = $2
                       AND tool_attempt.attempt_id =
                           entry.tool_result_attempt_id
                )
            )
          ORDER BY entry.payload_kind",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    assert_eq!(
        semantic_tool_kinds,
        vec![String::from("assistant_tool_use")]
    );
    Ok(())
}
