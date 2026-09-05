#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration tests use assertion panics and explicit fixture expectations"
)]

mod support;

use std::{
    error::Error,
    fmt, fs,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    CreateSessionOutcome, CreateSessionRequest, CreateSessionService, DecideToolRequestService,
    InProcessAttemptDispatchGate, InProcessEligibilityWorkSource, InProcessToolDispatchGate,
    ModelCallCredentialReference, OperatorFailureClass, StartEligibleTurnOutcome,
    StartEligibleTurnService, StartupScanService, SubmitInputOutcome, SubmitInputRequest,
    SubmitInputService, ToolCatalog, ToolCatalogValidationFailure, ToolDefinition,
    ToolExecutionInvocation, ToolExecutor, ToolExecutorEvidence, ToolInputSchema,
    ToolPreauthorization, UuidV7SessionIdGenerator, UuidV7StartEligibleTurnIdGenerator,
    UuidV7StartupScanIdGenerator, UuidV7SubmitInputIdGenerator, UuidV7ToolLoopIdGenerator,
};
use signalbox_domain::{
    ActivatedTurn, DangerousToolAutoApproval, DecideToolRequest, DecideToolRequestResult,
    DeliveryRequest, DescendantTerminationScope, DirectModelSelection, DurableCommandId,
    ModelCallId, ModelSelectionOverride, ModelSelectionRequest, ModelTargetCatalog,
    ModelTargetDefinition, NormalizedToolArguments, PerInputConfigurationChoices,
    ProviderModelIdentity, ResolvedProviderTarget, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SubmitInputAppliedResult,
    SubmitInputRejectedResult, SubmitInputResult, ToolApprovalDecision, ToolApprovalPosture,
    ToolAttemptDispatchCorrelation, ToolDispatchGeneration, ToolEffectClass,
    ToolExecutionErrorDetail, ToolName, ToolPermissionDefault, ToolRequestId, TurnId, UserContent,
};
use signalbox_model_provider_runtime::{
    ApprovalJudgeModel, RuntimeApprovalJudgeModel, RuntimeModelCallProvider, RuntimeModelCatalog,
    RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionEvidence, CompletionFinish, CredentialAccess,
    CredentialAccessError, CredentialReference, CredentialValue, ExchangeFacts, MessagePart,
    ModelOperation, ModelRuntime, NativeErrorFacts, ObservationSink, PreparationOutcome,
    ProviderErrorEvidence, ProviderErrorKind, ProviderReportedModel, RefusalEvidence, Script,
    ScriptedModel, ScriptedPrepared, TerminalEvidence, TerminalReport, TokenUsage, ToolCallId,
    ToolCallProposal as RuntimeToolCallProposal, ToolName as RuntimeToolName, ToolResultRecord,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository, disposable_postgres_server_args,
    disposable_postgres_state_tmpfs_from_example, disposable_test_container_labels,
    local_test_connection_options, migrate, model_execution::PostgresModelCallRepository,
    process_read::ProcessReadRepository, scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository, startup::PostgresStartupScanRepository,
    submit_input::SubmitInputRepository, tool_loop::PostgresToolLoopRepository,
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest, CommandId, InputDelivery,
    ModelSettingsOverlay, ProtocolVersion, RequestId, ServerMessage, ToolDecision,
    UserInputContent, decode_server_line, encode_client_line,
};
use signalbox_tools_exec::{
    BwrapAvailability, CaptureCompleteness, ProcessOutcome, ProcessOutput, ProcessRequest,
    ProcessRunResult, ProcessRunner, ProcessSpawnFailure, SANDBOXED_EXEC_NAME,
};
use signalbox_tools_git::{GIT_STATUS_NAME, GitIdentity};
use signalbox_tools_web::{
    WEB_FETCH_NAME, WebSearchRequest, WebSearchTransport, WebSearchTransportFailure,
    WebSearchTransportOutcome,
};
use signalboxd::{
    ActivatedTurnExecution, CHANGE_REQUEST_CHANGED_FILES_NAME, CHANGE_REQUEST_CHECKS_STATUS_NAME,
    CHANGE_REQUEST_CI_JOB_LOG_NAME, CHANGE_REQUEST_COMMENT_NAME,
    CHANGE_REQUEST_CONVERGENCE_STATE_NAME, CHANGE_REQUEST_FILE_PATCH_NAME,
    CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME, CHANGE_REQUEST_REVIEW_THREADS_NAME,
    CHANGE_REQUEST_STACK_STATE_NAME, CHANGE_REQUEST_SUMMARY_NAME,
    CHANGE_REQUEST_THREAD_INVENTORY_NAME, CHANGE_REQUEST_THREAD_REPLY_NAME,
    CHANGE_REQUEST_THREAD_RESOLVE_NAME, ChangeRequestCommentResult, ChangeRequestSummaryFields,
    ChangeRequestSummaryResult, ChangedFile, ChangedFilesResult, CheckStatus, ChecksStatusResult,
    CiJobLogResult, CodeHostNumericBounds, CodeHostOperation, CodeHostResult,
    CodeHostResultCompleteness, CodeHostTransport, CodeHostTransportFailure,
    ConvergenceStateFields, ConvergenceStateResult, ConversationIntrospectionPort,
    ConversationListPage, ConversationListRequest, ConversationTranscriptRead,
    ConversationTranscriptRequest, DaemonTools, DaemonToolsConstructionError, FilePatchResult,
    GitHubEgressPolicy, GitHubOperation, GitHubResult, GitHubTransport, GitHubTransportFailure,
    HubModelConfiguration, ImportedTranscriptRequest, LocalProcessListener,
    LocalWorkspaceFileSystem, MappedDaemonCredentialInputs, PULL_REQUEST_METADATA_NAME,
    PULL_REQUEST_PUBLISH_REVIEW_NAME, PostgresConversationIntrospection,
    PostgresProviderModelExecution, PostgresProviderToolLoopExecution, PostgresSessionStatusWriter,
    ProcessRuntime, READ_FILE_NAME, REVIEW_GATE_CHECK_NAME, RerunFailedJobsResult,
    ReviewAuthorClass, ReviewDispositionClass, ReviewGateCheckResult, ReviewGatePurpose,
    ReviewThread, ReviewThreadComment, ReviewThreadFields, ReviewThreadInventoryFields,
    ReviewThreadInventoryItem, ReviewThreadResolution, ReviewThreadsResult,
    ReviewerVerdictEvidence, ReviewerVerdictFields, ReviewerVerdictStatus, SessionStatusWrite,
    SessionStatusWriteOutcome, SessionStatusWriter, StackStateFields, StackStateResult,
    ThreadInventoryResult, ThreadReplyResult, ThreadResolveResult, TranscriptPage, WRITE_FILE_NAME,
    WebFetchBodyCompleteness, WebFetchEgressPolicy, WebFetchRequest, WebFetchResponse,
    WebFetchTransport, WebFetchTransportFailure,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use tempfile::tempdir;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::watch,
    time::timeout,
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalboxd_tool_loop_e2e";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const GIT_AUTHOR_NAME: &str = "Signalbox Daemon";
const GIT_AUTHOR_EMAIL: &str = "signalbox@example.test";
const OFFLINE_SANDBOX_LAUNCHER: &str = "/bin/sh";
const OFFLINE_SANDBOX_LAUNCHER_DESCRIPTOR: i32 = 3;

fn git_identity() -> GitIdentity {
    GitIdentity::try_new(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL).expect("fixture Git identity is valid")
}

fn test_session_credential_pin() -> signalbox_persistence::SessionCredentialPin {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        ),
    ])
    .expect("test credential pin is valid")
}
const FIXTURE_ID_SEED: u128 = 0x3100;
const DECISION_COMMAND_ID: u128 = 0x3110;
const OFFLINE_CODE_HOST_TOKEN: &[u8] = b"offline-code-host-token";

const fn code_host_bounds() -> CodeHostNumericBounds {
    CodeHostNumericBounds::new(None, None, None, None, None, None)
}
const FIXTURE_USER_CONTENT: &str = "offline tool-loop request";
const PROCESS_MODEL_CONFIGURATION: &str = r#"
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
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000002"
model_family = "anthropic"
provider_model = "fixture-model"
max_output_tokens = 64
context_window_tokens = 200000
"#;

fn approval_judge_model_configuration() -> HubModelConfiguration {
    support::parse_model_configuration(&format!(
        r#"
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
members = [{{ profile = "anthropic-primary", priority = 1 }}]


[[adapter_mappings]]
model_family = "fixture"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "Preserve the fixture."

[[models]]
selection_id = "{}"
target_id = "{}"
model_family = "fixture"
provider_model = "scripted-tool-loop"
max_output_tokens = 64
context_window_tokens = 200000
"#,
        Uuid::from_u128(FIXTURE_ID_SEED + 1),
        Uuid::from_u128(FIXTURE_ID_SEED + 4),
    ))
    .expect("the approval judge fixture model configuration is valid")
}

#[derive(Clone, Debug)]
struct RecordingScriptedModel {
    inner: Arc<ScriptedModel<ModelCallId>>,
    shutdown_after_execute: Option<watch::Sender<bool>>,
}

type FixtureProvider = RuntimeModelCallProvider<RecordingScriptedModel>;
type FixtureExecution<Catalog, Executor> =
    PostgresProviderToolLoopExecution<FixtureProvider, Catalog, Executor>;
type FixtureJudgeExecution<Catalog, Executor> = (
    FixtureExecution<Catalog, Executor>,
    Arc<ScriptedModel<ModelCallId>>,
    Arc<ScriptedModel<ModelCallId>>,
);

#[derive(Debug, sqlx::FromRow)]
struct DelegateDecisionProjection {
    decision_kind: String,
    decision_source: String,
    rationale: String,
    recommendation: String,
    model_call_matches: bool,
    model_selection_id: Uuid,
}

#[derive(Debug, sqlx::FromRow)]
struct EscalatedParkProjection {
    active_phase: String,
    approval_request_id: Uuid,
    recommendation: String,
    rationale: String,
    decision_count: i64,
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
        let report = self.inner.execute(prepared, sink, cancellation).await;
        if let Some(shutdown) = &self.shutdown_after_execute {
            shutdown.send_replace(true);
        }
        report
    }
}

struct ToolLoopFixture {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    session: SessionId,
    turn: TurnId,
    activated: ActivatedTurn,
    selection: DirectModelSelection,
    targets: ModelTargetCatalog,
    runtime_models: RuntimeModelCatalog,
    credential_reference: ModelCallCredentialReference,
    tool_dispatch_gate: InProcessToolDispatchGate,
}

#[derive(Debug, Eq, PartialEq, sqlx::FromRow)]
struct SessionMetadataRootFacts {
    title: String,
    archived: bool,
    actor_kind: String,
    actor_tool_request_id: Option<Uuid>,
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
            CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
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
                UserContent::try_text(String::from(FIXTURE_USER_CONTENT))
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
                200_000,
            )
            .expect("fixture runtime definition is valid")])
            .expect("one fixture runtime target is unique");

        Ok(Self {
            _container: container,
            pool,
            session,
            turn,
            activated: *activated,
            selection,
            targets,
            runtime_models,
            credential_reference: ModelCallCredentialReference::new("scripted-tool-loop-test"),
            tool_dispatch_gate,
        })
    }

    const fn selection(&self) -> DirectModelSelection {
        self.selection
    }

    fn execution<Catalog, Executor>(
        &self,
        scripts: impl IntoIterator<Item = Script>,
        catalog: Catalog,
        executor: Executor,
    ) -> (
        FixtureExecution<Catalog, Executor>,
        Arc<ScriptedModel<ModelCallId>>,
    )
    where
        Catalog: signalbox_application::ToolCatalog + Clone + Send + 'static,
        Executor: ToolExecutor + Clone + Send + 'static,
        Executor::Error: Send + 'static,
    {
        self.execution_with_model_shutdown(scripts, catalog, executor, None)
    }

    fn execution_requesting_shutdown<Catalog, Executor>(
        &self,
        scripts: impl IntoIterator<Item = Script>,
        catalog: Catalog,
        executor: Executor,
        shutdown: watch::Sender<bool>,
    ) -> (
        FixtureExecution<Catalog, Executor>,
        Arc<ScriptedModel<ModelCallId>>,
    )
    where
        Catalog: signalbox_application::ToolCatalog + Clone + Send + 'static,
        Executor: ToolExecutor + Clone + Send + 'static,
        Executor::Error: Send + 'static,
    {
        self.execution_with_model_shutdown(scripts, catalog, executor, Some(shutdown))
    }

    fn execution_with_model_shutdown<Catalog, Executor>(
        &self,
        scripts: impl IntoIterator<Item = Script>,
        catalog: Catalog,
        executor: Executor,
        shutdown_after_execute: Option<watch::Sender<bool>>,
    ) -> (
        FixtureExecution<Catalog, Executor>,
        Arc<ScriptedModel<ModelCallId>>,
    )
    where
        Catalog: signalbox_application::ToolCatalog + Clone + Send + 'static,
        Executor: ToolExecutor + Clone + Send + 'static,
        Executor::Error: Send + 'static,
    {
        self.execution_with_model_shutdown_and_limit(
            scripts,
            catalog,
            executor,
            shutdown_after_execute,
            None,
        )
    }

    fn execution_with_tool_round_limit<Catalog, Executor>(
        &self,
        scripts: impl IntoIterator<Item = Script>,
        catalog: Catalog,
        executor: Executor,
        automatic_tool_round_limit: Option<usize>,
    ) -> (
        FixtureExecution<Catalog, Executor>,
        Arc<ScriptedModel<ModelCallId>>,
    )
    where
        Catalog: signalbox_application::ToolCatalog + Clone + Send + 'static,
        Executor: ToolExecutor + Clone + Send + 'static,
        Executor::Error: Send + 'static,
    {
        self.execution_with_model_shutdown_and_limit(
            scripts,
            catalog,
            executor,
            None,
            automatic_tool_round_limit,
        )
    }

    fn execution_with_model_shutdown_and_limit<Catalog, Executor>(
        &self,
        scripts: impl IntoIterator<Item = Script>,
        catalog: Catalog,
        executor: Executor,
        shutdown_after_execute: Option<watch::Sender<bool>>,
        automatic_tool_round_limit: Option<usize>,
    ) -> (
        FixtureExecution<Catalog, Executor>,
        Arc<ScriptedModel<ModelCallId>>,
    )
    where
        Catalog: signalbox_application::ToolCatalog + Clone + Send + 'static,
        Executor: ToolExecutor + Clone + Send + 'static,
        Executor::Error: Send + 'static,
    {
        let runtime = Arc::new(ScriptedModel::<ModelCallId>::following(scripts));
        let provider = RuntimeModelCallProvider::new(
            RecordingScriptedModel {
                inner: Arc::clone(&runtime),
                shutdown_after_execute,
            },
            self.runtime_models.clone(),
            None,
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
                automatic_tool_round_limit,
            )
            .with_tool_loop(self.tool_dispatch_gate.clone(), catalog, executor)
            .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
                self.pool.clone(),
                None,
                Vec::new(),
            )),
            runtime,
        )
    }

    fn execution_with_judge<Catalog, Executor>(
        &self,
        scripts: impl IntoIterator<Item = Script>,
        judge_script: Script,
        catalog: Catalog,
        executor: Executor,
    ) -> FixtureJudgeExecution<Catalog, Executor>
    where
        Catalog: signalbox_application::ToolCatalog + Clone + Send + 'static,
        Executor: ToolExecutor + Clone + Send + 'static,
        Executor::Error: Send + 'static,
    {
        let runtime = Arc::new(ScriptedModel::<ModelCallId>::following(scripts));
        let judge_runtime = Arc::new(ScriptedModel::<ModelCallId>::single(judge_script));
        let provider = RuntimeModelCallProvider::new(
            RecordingScriptedModel {
                inner: Arc::clone(&runtime),
                shutdown_after_execute: None,
            },
            self.runtime_models.clone(),
            None,
        );
        let judge: Arc<dyn ApprovalJudgeModel> = Arc::new(RuntimeApprovalJudgeModel::new(
            RecordingScriptedModel {
                inner: Arc::clone(&judge_runtime),
                shutdown_after_execute: None,
            },
            self.runtime_models.clone(),
        ));
        let configuration = approval_judge_model_configuration();
        (
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    self.pool.clone(),
                    self.targets.clone(),
                    self.credential_reference.clone(),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            )
            .with_tool_loop(self.tool_dispatch_gate.clone(), catalog, executor)
            .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
                self.pool.clone(),
                None,
                Vec::new(),
            ))
            .with_approval_judge(judge, None, configuration),
            runtime,
            judge_runtime,
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
            .execute(
                DecideToolRequest::try_new(
                    DurableCommandId::from_uuid(Uuid::from_u128(DECISION_COMMAND_ID)),
                    request,
                    decision,
                )
                .expect("fixture decision reason is admitted"),
            )
            .await?;
        assert!(
            matches!(prepared.result(), DecideToolRequestResult::Applied(_)),
            "the earliest undecided request must accept its user decision"
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

    async fn submit_new_turn(
        &self,
        command: u128,
        content: &str,
    ) -> Result<TurnId, Box<dyn Error>> {
        let sweep = PostgresEligibilitySweep::new(self.pool.clone());
        let (nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
        let mut submit = SubmitInputService::new(
            UuidV7SubmitInputIdGenerator,
            SubmitInputRepository::new(self.pool.clone()),
            nudge,
            self.tool_dispatch_gate.clone(),
        );
        let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(origin),
        )) = submit
            .execute(SubmitInputRequest::try_new(
                DurableCommandId::from_uuid(Uuid::from_u128(command)),
                self.session,
                UserContent::try_text(content.to_owned())
                    .expect("follow-up fixture content is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: default_configuration(),
                },
            )?)
            .await?
        else {
            panic!("terminal tool history must admit a new queued turn")
        };
        Ok(origin.turn())
    }

    async fn activate_and_complete_turn(
        &self,
        expected_turn: TurnId,
        response: &str,
    ) -> Result<(), Box<dyn Error>> {
        let mut start = StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(self.pool.clone()),
        );
        let StartEligibleTurnOutcome::Activated(activated) = start.execute(self.session).await?
        else {
            panic!("the expected queued follow-up turn must activate")
        };
        assert_eq!(activated.turn(), expected_turn);

        let (execution, runtime) = self.execution(
            [completion_script(response)],
            catalog(std::iter::empty::<CompiledTool>()),
            RecordingExecutor::completing(),
        );
        execution.execute(activated).await?;
        assert_eq!(
            runtime.received_operations().len(),
            1,
            "the activated follow-up must run exactly one model call"
        );
        let terminal_shape: (String, i64) = sqlx::query_as(
            "SELECT terminal_disposition_kind,
                    (SELECT count(*) FROM model_call
                      WHERE session_id = $1 AND turn_id = $2)
               FROM turn_lifecycle
              WHERE session_id = $1
                AND turn_id = $2",
        )
        .bind(self.session.into_uuid())
        .bind(expected_turn.into_uuid())
        .fetch_one(&self.pool)
        .await?;
        assert_eq!(terminal_shape, (String::from("completed"), 1));
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

fn delegated_tool(name: &str, effect: ToolEffectClass) -> CompiledTool {
    let definition = ToolDefinition::new(
        tool_name(name),
        format!("Runs the {name} fixture tool."),
        ToolInputSchema::try_new(String::from(
            r#"{"additionalProperties":true,"type":"object"}"#,
        ))
        .expect("fixture schema is valid"),
        ToolPermissionDefault::Confirm,
        effect,
    )
    .with_approval_posture(ToolApprovalPosture::Delegated);
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

fn approval_judge_script(recommendation: &str, rationale: &str) -> Script {
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new("scripted-tool-loop")),
        finish: CompletionFinish::ToolUse,
        content: vec![AssistantPart::ToolCall(RuntimeToolCallProposal {
            id: ToolCallId::new("fixture-approval-judge-call"),
            name: RuntimeToolName::new("tool_approval_decision"),
            arguments_json: serde_json::json!({
                "recommendation": recommendation,
                "rationale": rationale,
            })
            .to_string(),
        })],
        usage: TokenUsage {
            input_tokens: Some(17),
            output_tokens: Some(7),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(2),
        },
    }))
}

async fn model_call_history_count(
    pool: &PgPool,
    session: SessionId,
) -> Result<usize, Box<dyn Error>> {
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .ok_or_else(|| std::io::Error::other("fixture session is absent"))?;
    Ok(snapshot.model_call_usage().len())
}

fn provider_error_script() -> Script {
    Script::delivering(TerminalEvidence::ProviderError(ProviderErrorEvidence {
        exchange: ExchangeFacts::default(),
        reported_model: Some(ProviderReportedModel::new("scripted-tool-loop")),
        kind: ProviderErrorKind::ProviderInternal,
        non_acceptance_proven: false,
        native: NativeErrorFacts::default(),
        usage: TokenUsage::unreported(),
    }))
}

fn refusal_script() -> Script {
    Script::delivering(TerminalEvidence::Refused(RefusalEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new("scripted-tool-loop")),
        content: Vec::new(),
        usage: TokenUsage::unreported(),
    }))
}

fn continuation_tool_exchange(
    runtime: &ScriptedModel<ModelCallId>,
) -> Result<Vec<MessagePart>, Box<dyn Error>> {
    let operations = runtime.received_operations();
    let continuation = operations
        .last()
        .ok_or_else(|| std::io::Error::other("continuation model operation was not received"))?;
    Ok(continuation
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter(|part| matches!(part, MessagePart::ToolCall(_) | MessagePart::ToolResult(_)))
        .cloned()
        .collect())
}

fn continuation_result_json(
    runtime: &ScriptedModel<ModelCallId>,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let result = continuation_tool_exchange(runtime)?
        .into_iter()
        .find_map(|part| match part {
            MessagePart::ToolResult(result) => Some(result.content),
            _ => None,
        })
        .ok_or_else(|| std::io::Error::other("continuation carried no tool result"))?;
    Ok(serde_json::from_str(&result)?)
}

#[track_caller]
fn assert_commissioned_catalog(operation: &ModelOperation<ModelCallId>, expected_names: &[String]) {
    let names = operation
        .tools
        .iter()
        .map(|definition| definition.name.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(names, expected_names);
}

fn commissioned_catalog_names() -> Vec<String> {
    [
        "apply_patch",
        "await_session",
        "cargo_diagnostics",
        "change_request_changed_files",
        "change_request_checks_status",
        "change_request_ci_job_log",
        "change_request_comment",
        "change_request_convergence_state",
        "change_request_file_patch",
        "change_request_rerun_failed_jobs",
        "change_request_review_threads",
        "change_request_stack_state",
        "change_request_summary",
        "change_request_thread_inventory",
        "change_request_thread_reply",
        "change_request_thread_resolve",
        "current_time",
        "echo",
        "edit_file",
        "git_branch_create",
        "git_branch_switch",
        "git_create_commit",
        "git_diff",
        "git_log",
        "git_stage",
        "git_status",
        "github_pull_request_diff",
        "github_pull_request_metadata",
        "github_pull_request_publish_review",
        "github_pull_request_review_threads",
        "glob_files",
        "list_conversations",
        "list_directory",
        "plan_read",
        "plan_write",
        "read_conversation",
        "read_file",
        "read_imported_conversation",
        "read_own_conversation",
        "repository_list_directory",
        "repository_read_file",
        "review_gate_check",
        "sandboxed_exec",
        "search_files",
        "send_session_message",
        "session_status_update",
        "spawn_session",
        "unsandboxed_exec",
        "web_fetch",
        "web_search",
        "write_file",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn expected_tool_call(request: ToolRequestId, name: &str, arguments_json: &str) -> MessagePart {
    MessagePart::ToolCall(RuntimeToolCallProposal {
        id: ToolCallId::new(request.into_uuid().to_string()),
        name: RuntimeToolName::new(name),
        arguments_json: arguments_json.to_owned(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedToolResultDisposition {
    Successful,
    Failed,
}

fn expected_tool_result(
    request: ToolRequestId,
    content: String,
    disposition: ExpectedToolResultDisposition,
) -> MessagePart {
    MessagePart::ToolResult(ToolResultRecord {
        tool_call_id: ToolCallId::new(request.into_uuid().to_string()),
        content,
        is_error: matches!(disposition, ExpectedToolResultDisposition::Failed),
    })
}

fn expected_successful_tool_result(request: ToolRequestId, content: String) -> MessagePart {
    expected_tool_result(request, content, ExpectedToolResultDisposition::Successful)
}

fn expected_failed_tool_result(request: ToolRequestId, content: String) -> MessagePart {
    expected_tool_result(request, content, ExpectedToolResultDisposition::Failed)
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
    correlations: Arc<Mutex<Vec<ToolAttemptDispatchCorrelation>>>,
    shutdown: Option<watch::Sender<bool>>,
}

impl RecordingExecutor {
    fn completing() -> Self {
        Self {
            mode: ExecutorMode::Complete,
            events: Arc::new(Mutex::new(Vec::new())),
            arguments: Arc::new(Mutex::new(Vec::new())),
            correlations: Arc::new(Mutex::new(Vec::new())),
            shutdown: None,
        }
    }

    fn completing_and_requesting_shutdown(shutdown: watch::Sender<bool>) -> Self {
        Self {
            mode: ExecutorMode::Complete,
            events: Arc::new(Mutex::new(Vec::new())),
            arguments: Arc::new(Mutex::new(Vec::new())),
            correlations: Arc::new(Mutex::new(Vec::new())),
            shutdown: Some(shutdown),
        }
    }

    fn losing_process() -> Self {
        Self {
            mode: ExecutorMode::LoseProcess,
            events: Arc::new(Mutex::new(Vec::new())),
            arguments: Arc::new(Mutex::new(Vec::new())),
            correlations: Arc::new(Mutex::new(Vec::new())),
            shutdown: None,
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

    fn correlations(&self) -> Vec<ToolAttemptDispatchCorrelation> {
        self.correlations
            .lock()
            .expect("fixture correlation lock is available")
            .clone()
    }
}

/// Requests daemon shutdown whenever the tool loop resolves one exact tool
/// name, and otherwise answers exactly as the wrapped catalog does.
///
/// Only the loop's own attempt preparation and its preflight resolve a single
/// name; everything earlier reads the advertised snapshot. So the first such
/// lookup of a batch is the one preceding that batch's attempt checkpoint, and
/// the request lands between two committed boundaries rather than inside an
/// issued operation — the interleaving an asynchronous `SIGTERM` produces, and
/// the one the drive loop's shutdown checks exist to catch.
#[derive(Clone, Debug)]
struct ShutdownOnToolResolutionCatalog {
    inner: CompiledToolCatalog,
    shutdown: watch::Sender<bool>,
}

impl ShutdownOnToolResolutionCatalog {
    const fn new(inner: CompiledToolCatalog, shutdown: watch::Sender<bool>) -> Self {
        Self { inner, shutdown }
    }
}

impl ToolCatalog for ShutdownOnToolResolutionCatalog {
    fn definitions(&self) -> Box<[ToolDefinition]> {
        self.inner.definitions()
    }

    fn definition(&self, name: &ToolName) -> Option<ToolDefinition> {
        self.shutdown.send_replace(true);
        self.inner.definition(name)
    }

    fn validate_arguments(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolCatalogValidationFailure> {
        self.inner.validate_arguments(name, arguments)
    }

    fn preauthorization(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<ToolPreauthorization, ToolCatalogValidationFailure> {
        self.inner.preauthorization(name, arguments)
    }
}

#[derive(Clone, Debug)]
struct SerialProbeExecutor {
    events: Arc<Mutex<Vec<String>>>,
    correlations: Arc<Mutex<Vec<ToolAttemptDispatchCorrelation>>>,
    first_entered: Arc<tokio::sync::Notify>,
    release_first: Arc<tokio::sync::Notify>,
}

struct FirstExecutionRelease<'a>(&'a SerialProbeExecutor);

impl Drop for FirstExecutionRelease<'_> {
    fn drop(&mut self) {
        self.0.release_first();
    }
}

impl SerialProbeExecutor {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            correlations: Arc::new(Mutex::new(Vec::new())),
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

    fn correlations(&self) -> Vec<ToolAttemptDispatchCorrelation> {
        self.correlations
            .lock()
            .expect("fixture correlation lock is available")
            .clone()
    }

    async fn wait_for_first(&self) -> Result<(), tokio::time::error::Elapsed> {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.first_entered.notified(),
        )
        .await
    }

    async fn assert_first_only_then_release(
        &self,
        expected: &str,
    ) -> Result<(), tokio::time::error::Elapsed> {
        let _release = FirstExecutionRelease(self);
        self.wait_for_first().await?;
        self.assert_only_event(expected);
        Ok(())
    }

    #[track_caller]
    fn assert_only_event(&self, expected: &str) {
        assert_eq!(
            self.events(),
            vec![expected.to_owned()],
            "the second executor must not enter while the first remains pending"
        );
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

#[derive(Clone, Debug)]
struct OfflineWebTransport {
    response: Result<WebFetchResponse, WebFetchTransportFailure>,
    requests: Arc<Mutex<Vec<WebFetchRequest>>>,
}

impl OfflineWebTransport {
    fn responding(response: WebFetchResponse) -> Self {
        Self {
            response: Ok(response),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn unused() -> Self {
        Self {
            response: Err(WebFetchTransportFailure::RequestFailed),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<WebFetchRequest> {
        self.requests
            .lock()
            .expect("fixture web-request lock is available")
            .clone()
    }
}

impl WebFetchTransport for OfflineWebTransport {
    async fn fetch(
        &mut self,
        request: WebFetchRequest,
    ) -> Result<WebFetchResponse, WebFetchTransportFailure> {
        self.requests
            .lock()
            .expect("fixture web-request lock is available")
            .push(request);
        self.response.clone()
    }
}

#[derive(Clone, Copy, Debug)]
struct UnusedWebSearchTransport;

impl WebSearchTransport for UnusedWebSearchTransport {
    async fn search(
        &mut self,
        _request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        WebSearchTransportOutcome::failed(WebSearchTransportFailure::RequestFailed, credential)
    }
}

#[derive(Clone, Copy, Debug)]
struct OfflineCodeHostCredentials;

impl CredentialAccess for OfflineCodeHostCredentials {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        Ok(CredentialValue::new(OFFLINE_CODE_HOST_TOKEN.to_vec()))
    }
}

#[derive(Clone, Copy, Debug)]
struct UnusedCodeHostTransport;

impl CodeHostTransport for UnusedCodeHostTransport {
    fn numeric_bounds(&self) -> CodeHostNumericBounds {
        code_host_bounds()
    }

    async fn execute(
        &mut self,
        _operation: CodeHostOperation,
        _credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        Err(CodeHostTransportFailure::Rejected)
    }
}

#[derive(Clone, Copy, Debug)]
struct UnusedGitHubTransport;

impl GitHubTransport for UnusedGitHubTransport {
    async fn execute(
        &mut self,
        _operation: GitHubOperation,
        _credential: &CredentialValue,
        _egress_policy: &GitHubEgressPolicy,
    ) -> Result<GitHubResult, GitHubTransportFailure> {
        Err(GitHubTransportFailure::PreDispatchInfrastructure)
    }
}

#[derive(Clone, Copy, Debug)]
struct UnusedConversationPort;

impl ConversationIntrospectionPort for UnusedConversationPort {
    type Error = UnusedSessionStatusWriterError;

    async fn list_conversations(
        &mut self,
        _request: ConversationListRequest,
    ) -> Result<ConversationListPage, Self::Error> {
        Ok(ConversationListPage::new(Vec::new(), false))
    }

    async fn read_conversation(
        &mut self,
        _request: ConversationTranscriptRequest,
    ) -> Result<ConversationTranscriptRead, Self::Error> {
        Ok(ConversationTranscriptRead::NotFound)
    }

    async fn read_imported_conversation(
        &mut self,
        _request: ImportedTranscriptRequest,
    ) -> Result<Option<TranscriptPage>, Self::Error> {
        Ok(None)
    }
}

type OfflineDaemonTools<Writer, HostTransport> = DaemonTools<
    fn() -> SystemTime,
    OfflineWebTransport,
    UnusedWebSearchTransport,
    Writer,
    OfflineCodeHostCredentials,
    HostTransport,
    UnusedGitHubTransport,
    LocalWorkspaceFileSystem,
    UnusedConversationPort,
    UnusedConversationPort,
    OfflineProcessRunner,
>;

#[derive(Clone, Copy, Debug)]
struct OfflineProcessRunner;

impl ProcessRunner for OfflineProcessRunner {
    fn sandbox_launcher_program(&self) -> &std::path::Path {
        std::path::Path::new(OFFLINE_SANDBOX_LAUNCHER)
    }

    fn sandbox_launcher_descriptor(&self) -> Option<i32> {
        Some(OFFLINE_SANDBOX_LAUNCHER_DESCRIPTOR)
    }

    async fn bwrap_availability(&mut self, _probe: ProcessRequest) -> BwrapAvailability {
        BwrapAvailability::Unusable
    }

    async fn run(&mut self, _request: ProcessRequest) -> ProcessRunResult {
        ProcessRunResult {
            outcome: ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::Other,
            },
            stdout: ProcessOutput {
                bytes: Vec::new(),
                completeness: CaptureCompleteness::Complete,
            },
            stderr: ProcessOutput {
                bytes: Vec::new(),
                completeness: CaptureCompleteness::Complete,
            },
        }
    }
}
fn offline_daemon_tools<Writer, HostTransport>(
    web: OfflineWebTransport,
    writer: Writer,
    code_host: HostTransport,
    web_fetch_egress_policy: WebFetchEgressPolicy,
) -> Result<OfflineDaemonTools<Writer, HostTransport>, DaemonToolsConstructionError> {
    fn epoch() -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    let workspace = tempdir().expect("fixture workspace root exists");
    git2::Repository::init(workspace.path()).expect("fixture repository initializes");
    DaemonTools::try_new(
        epoch as fn() -> SystemTime,
        web,
        MappedDaemonCredentialInputs {
            web_search: OfflineCodeHostCredentials,
            code_host: OfflineCodeHostCredentials,
            github: OfflineCodeHostCredentials,
        },
        UnusedWebSearchTransport,
        writer,
        code_host,
        UnusedGitHubTransport,
        GitHubEgressPolicy::github_api_only(),
        LocalWorkspaceFileSystem,
        workspace.path(),
        git_identity(),
        OfflineProcessRunner,
        UnusedConversationPort,
        UnusedConversationPort,
        web_fetch_egress_policy,
    )
}
#[derive(Clone, Debug)]
struct RecordingGitHubTransport {
    result: GitHubResult,
    operations: Arc<Mutex<Vec<GitHubOperation>>>,
    credential_matches: Arc<Mutex<Vec<bool>>>,
    policy_matches: Arc<Mutex<Vec<bool>>>,
}

impl RecordingGitHubTransport {
    fn responding(result: GitHubResult) -> Self {
        Self {
            result,
            operations: Arc::new(Mutex::new(Vec::new())),
            credential_matches: Arc::new(Mutex::new(Vec::new())),
            policy_matches: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn operations(&self) -> Vec<GitHubOperation> {
        self.operations
            .lock()
            .expect("fixture GitHub operation lock is available")
            .clone()
    }

    fn credential_matches(&self) -> Vec<bool> {
        self.credential_matches
            .lock()
            .expect("fixture GitHub credential lock is available")
            .clone()
    }

    fn policy_matches(&self) -> Vec<bool> {
        self.policy_matches
            .lock()
            .expect("fixture GitHub policy lock is available")
            .clone()
    }
}

impl GitHubTransport for RecordingGitHubTransport {
    async fn execute(
        &mut self,
        operation: GitHubOperation,
        credential: &CredentialValue,
        policy: &GitHubEgressPolicy,
    ) -> Result<GitHubResult, GitHubTransportFailure> {
        self.operations
            .lock()
            .expect("fixture GitHub operation lock is available")
            .push(operation);
        self.credential_matches
            .lock()
            .expect("fixture GitHub credential lock is available")
            .push(credential.expose_bytes() == OFFLINE_CODE_HOST_TOKEN);
        self.policy_matches
            .lock()
            .expect("fixture GitHub policy lock is available")
            .push(policy.admitted_origin() == "https://api.github.com");
        Ok(self.result.clone())
    }
}

type CommissionedDaemonTools<HostTransport, GitHubTransportType> = DaemonTools<
    fn() -> SystemTime,
    OfflineWebTransport,
    UnusedWebSearchTransport,
    UnusedSessionStatusWriter,
    OfflineCodeHostCredentials,
    HostTransport,
    GitHubTransportType,
    LocalWorkspaceFileSystem,
    PostgresConversationIntrospection,
    signalbox_persistence::plan::SessionPlanRepository,
    OfflineProcessRunner,
>;
fn commissioned_daemon_tools<HostTransport, GitHubTransportType>(
    pool: &PgPool,
    code_host: HostTransport,
    github: GitHubTransportType,
    workspace_root: &std::path::Path,
) -> Result<CommissionedDaemonTools<HostTransport, GitHubTransportType>, DaemonToolsConstructionError>
{
    fn epoch() -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    git2::Repository::init(workspace_root).expect("fixture repository initializes");
    DaemonTools::try_new(
        epoch as fn() -> SystemTime,
        OfflineWebTransport::unused(),
        MappedDaemonCredentialInputs {
            web_search: OfflineCodeHostCredentials,
            code_host: OfflineCodeHostCredentials,
            github: OfflineCodeHostCredentials,
        },
        UnusedWebSearchTransport,
        UnusedSessionStatusWriter,
        code_host,
        github,
        GitHubEgressPolicy::github_api_only(),
        LocalWorkspaceFileSystem,
        workspace_root,
        git_identity(),
        OfflineProcessRunner,
        PostgresConversationIntrospection::new(pool.clone()),
        signalbox_persistence::plan::SessionPlanRepository::new(pool.clone()),
        WebFetchEgressPolicy::deny_all(),
    )
}
#[derive(Clone, Debug)]
struct RecordingCodeHostTransport {
    result: CodeHostResult,
    operations: Arc<Mutex<Vec<CodeHostOperation>>>,
    credential_matches: Arc<Mutex<Vec<bool>>>,
}

impl RecordingCodeHostTransport {
    fn responding(result: CodeHostResult) -> Self {
        Self {
            result,
            operations: Arc::new(Mutex::new(Vec::new())),
            credential_matches: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn operations(&self) -> Vec<CodeHostOperation> {
        self.operations
            .lock()
            .expect("fixture code-host operation lock is available")
            .clone()
    }

    fn credential_matches(&self) -> Vec<bool> {
        self.credential_matches
            .lock()
            .expect("fixture credential-observation lock is available")
            .clone()
    }
}

impl CodeHostTransport for RecordingCodeHostTransport {
    fn numeric_bounds(&self) -> CodeHostNumericBounds {
        code_host_bounds()
    }

    async fn execute(
        &mut self,
        operation: CodeHostOperation,
        credential: &CredentialValue,
    ) -> Result<CodeHostResult, CodeHostTransportFailure> {
        self.operations
            .lock()
            .expect("fixture code-host operation lock is available")
            .push(operation);
        self.credential_matches
            .lock()
            .expect("fixture credential-observation lock is available")
            .push(credential.expose_bytes() == OFFLINE_CODE_HOST_TOKEN);
        Ok(self.result.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedCodeHostApproval {
    Auto,
    Confirm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedCodeHostOperation {
    Summary {
        repository: &'static str,
        number: u32,
    },
    ChangedFiles {
        repository: &'static str,
        number: u32,
    },
    FilePatch {
        repository: &'static str,
        number: u32,
        path: &'static str,
    },
    ChecksStatus {
        repository: &'static str,
        revision: &'static str,
    },
    Comment {
        repository: &'static str,
        number: u32,
        body: &'static str,
    },
    ReviewThreads {
        repository: &'static str,
        number: u32,
    },
    ConvergenceState {
        repository: &'static str,
        number: u32,
    },
    StackState {
        repository: &'static str,
        number: u32,
        cursor: Option<&'static str>,
    },
    ThreadInventory {
        repository: &'static str,
        number: u32,
        cursor: Option<&'static str>,
    },
    ReviewGateCheck {
        repository: &'static str,
        number: u32,
        purpose: ReviewGatePurpose,
    },
    ThreadReply {
        repository: &'static str,
        number: u32,
        thread_id: &'static str,
        body: &'static str,
    },
    ThreadResolve {
        repository: &'static str,
        number: u32,
        thread_id: &'static str,
    },
    CiJobLog {
        repository: &'static str,
        job_id: u64,
    },
    RerunFailedJobs {
        repository: &'static str,
        run_id: u64,
    },
}

#[track_caller]
fn assert_code_host_operation(actual: &CodeHostOperation, expected: ExpectedCodeHostOperation) {
    match (actual, expected) {
        (
            CodeHostOperation::Summary(arguments),
            ExpectedCodeHostOperation::Summary { repository, number },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
        }
        (
            CodeHostOperation::ChangedFiles(arguments),
            ExpectedCodeHostOperation::ChangedFiles { repository, number },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
        }
        (
            CodeHostOperation::FilePatch(arguments),
            ExpectedCodeHostOperation::FilePatch {
                repository,
                number,
                path,
            },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
            assert_eq!(arguments.path().as_str(), path);
        }
        (
            CodeHostOperation::ChecksStatus(arguments),
            ExpectedCodeHostOperation::ChecksStatus {
                repository,
                revision,
            },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.revision().as_str(), revision);
        }
        (
            CodeHostOperation::Comment(arguments),
            ExpectedCodeHostOperation::Comment {
                repository,
                number,
                body,
            },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
            assert_eq!(arguments.body().as_str(), body);
        }
        (
            CodeHostOperation::ReviewThreads(arguments),
            ExpectedCodeHostOperation::ReviewThreads { repository, number },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
        }
        (
            CodeHostOperation::ConvergenceState(arguments),
            ExpectedCodeHostOperation::ConvergenceState { repository, number },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
        }
        (
            CodeHostOperation::StackState(arguments),
            ExpectedCodeHostOperation::StackState {
                repository,
                number,
                cursor,
            },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
            assert_eq!(arguments.cursor().map(|value| value.as_str()), cursor);
        }
        (
            CodeHostOperation::ThreadInventory(arguments),
            ExpectedCodeHostOperation::ThreadInventory {
                repository,
                number,
                cursor,
            },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
            assert_eq!(arguments.cursor().map(|cursor| cursor.as_str()), cursor);
        }
        (
            CodeHostOperation::ReviewGateCheck(arguments),
            ExpectedCodeHostOperation::ReviewGateCheck {
                repository,
                number,
                purpose,
            },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
            assert_eq!(arguments.purpose(), purpose);
        }
        (
            CodeHostOperation::ThreadReply(arguments),
            ExpectedCodeHostOperation::ThreadReply {
                repository,
                number,
                thread_id,
                body,
            },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
            assert_eq!(arguments.thread_id().as_str(), thread_id);
            assert_eq!(arguments.body().as_str(), body);
        }
        (
            CodeHostOperation::ThreadResolve(arguments),
            ExpectedCodeHostOperation::ThreadResolve {
                repository,
                number,
                thread_id,
            },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.number().get(), number);
            assert_eq!(arguments.thread_id().as_str(), thread_id);
        }
        (
            CodeHostOperation::CiJobLog(arguments),
            ExpectedCodeHostOperation::CiJobLog { repository, job_id },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.job_id(), job_id);
        }
        (
            CodeHostOperation::RerunFailedJobs(arguments),
            ExpectedCodeHostOperation::RerunFailedJobs { repository, run_id },
        ) => {
            assert_eq!(arguments.repository().as_str(), repository);
            assert_eq!(arguments.run_id(), run_id);
        }
        _ => panic!("typed code-host operation did not match the invoked registry tool"),
    }
}

/// Runs one code-host tool through the durable loop against a mocked
/// transport. `expected_result` is the persisted tool-result JSON stated
/// independently of the serializer under test.
async fn code_host_tool_completes_offline(
    name: &'static str,
    arguments: String,
    result: CodeHostResult,
    expected_result: serde_json::Value,
    expected_operation: ExpectedCodeHostOperation,
    approval: ExpectedCodeHostApproval,
) -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let web = OfflineWebTransport::unused();
    let code_host = RecordingCodeHostTransport::responding(result);
    let (tool_catalog, tool_executor) = offline_daemon_tools(
        web.clone(),
        UnusedSessionStatusWriter,
        code_host.clone(),
        WebFetchEgressPolicy::deny_all(),
    )?
    .into_parts();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(name, arguments.as_str())]),
            completion_script("code-host result observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.wait_for_requests(1).await?[0];
    if approval == ExpectedCodeHostApproval::Confirm {
        assert!(
            code_host.operations().is_empty(),
            "confirmed code-host mutations cannot dispatch before user approval"
        );
        let receipt = approve_through_process(&fixture, request, 0x3c01).await?;
        assert_approved_receipt(receipt, request);
        execution.resume_active(fixture.session).await?;
    }
    let operations = code_host.operations();
    let [operation] = operations.as_slice() else {
        panic!("one physical code-host operation crosses the mocked transport")
    };

    assert_code_host_operation(operation, expected_operation);
    assert_eq!(code_host.credential_matches(), vec![true]);
    assert_eq!(
        continuation_tool_exchange(&runtime)?,
        vec![
            expected_tool_call(request, name, &arguments),
            expected_successful_tool_result(request, expected_result.to_string()),
        ]
    );
    assert!(
        web.requests().is_empty(),
        "code-host tools must not enter the credential-free web transport"
    );
    Ok(())
}

fn summary_result() -> CodeHostResult {
    CodeHostResult::Summary(
        ChangeRequestSummaryResult::try_new(
            code_host_bounds(),
            ChangeRequestSummaryFields {
                number: 17,
                title: format!(
                    "summary {}",
                    std::str::from_utf8(OFFLINE_CODE_HOST_TOKEN)
                        .expect("fixture credential is valid UTF-8")
                ),
                body: Some(String::from("bounded body")),
                state: String::from("open"),
                draft: false,
                author: Some(String::from("reviewer")),
                base_ref: String::from("main"),
                head_ref: String::from("feature"),
                head_revision: String::from("0123456789abcdef0123456789abcdef01234567"),
                url: String::from("https://github.example/owner/repository/pull/17"),
            },
        )
        .expect("fixture summary is bounded"),
    )
}

fn changed_files_result() -> CodeHostResult {
    CodeHostResult::ChangedFiles(
        ChangedFilesResult::try_new(
            code_host_bounds(),
            vec![
                ChangedFile::try_new(
                    code_host_bounds(),
                    String::from("src/lib.rs"),
                    String::from("modified"),
                    7,
                    2,
                )
                .expect("fixture changed file is bounded"),
            ],
            CodeHostResultCompleteness::Complete,
        )
        .expect("fixture changed-file page is bounded"),
    )
}

fn file_patch_result() -> CodeHostResult {
    CodeHostResult::FilePatch(
        FilePatchResult::try_new(
            code_host_bounds(),
            ChangedFile::try_new(
                code_host_bounds(),
                String::from("src/lib.rs"),
                String::from("modified"),
                7,
                2,
            )
            .expect("fixture changed file is bounded"),
            Some(String::from("@@ -1 +1 @@\n-old\n+new")),
        )
        .expect("fixture patch is bounded"),
    )
}

fn checks_status_result() -> CodeHostResult {
    CodeHostResult::ChecksStatus(
        ChecksStatusResult::try_new(
            code_host_bounds(),
            String::from("0123456789abcdef0123456789abcdef01234567"),
            vec![
                CheckStatus::try_new(
                    code_host_bounds(),
                    9001,
                    String::from("validate"),
                    String::from("completed"),
                    Some(String::from("success")),
                    String::from("https://github.example/check/9001"),
                )
                .expect("fixture check is bounded"),
            ],
            CodeHostResultCompleteness::Complete,
        )
        .expect("fixture checks page is bounded"),
    )
}

fn comment_result() -> CodeHostResult {
    CodeHostResult::Comment(
        ChangeRequestCommentResult::try_new(
            8001,
            String::from("https://github.example/comment/8001"),
        )
        .expect("fixture comment result is bounded"),
    )
}

fn review_threads_result() -> CodeHostResult {
    let comment = ReviewThreadComment::try_new(
        code_host_bounds(),
        String::from("PRRC_comment"),
        Some(String::from("reviewer")),
        String::from("please adjust"),
        String::from("https://github.example/comment/7001"),
    )
    .expect("fixture review comment is bounded");
    let thread = ReviewThread::try_new(
        code_host_bounds(),
        ReviewThreadFields {
            id: String::from("PRRT_thread"),
            resolved: false,
            outdated: false,
            path: String::from("src/lib.rs"),
            line: Some(12),
            comments: vec![comment],
            comments_truncated: false,
        },
    )
    .expect("fixture review thread is bounded");
    CodeHostResult::ReviewThreads(
        ReviewThreadsResult::try_new(
            code_host_bounds(),
            vec![thread],
            CodeHostResultCompleteness::Complete,
        )
        .expect("fixture review-thread page is bounded"),
    )
}

fn thread_reply_result() -> CodeHostResult {
    CodeHostResult::ThreadReply(
        ThreadReplyResult::try_new(
            String::from("PRRC_reply"),
            String::from("https://github.example/comment/7002"),
        )
        .expect("fixture reply result is bounded"),
    )
}

fn thread_resolve_result() -> CodeHostResult {
    CodeHostResult::ThreadResolve(
        ThreadResolveResult::try_new(
            code_host_bounds(),
            String::from("PRRT_thread"),
            ReviewThreadResolution::Resolved,
        )
        .expect("fixture resolve result is bounded"),
    )
}

fn ci_job_log_result() -> CodeHostResult {
    CodeHostResult::CiJobLog(
        CiJobLogResult::try_new(
            code_host_bounds(),
            9001,
            String::from("offline job log"),
            CodeHostResultCompleteness::Complete,
        )
        .expect("fixture job log is bounded"),
    )
}

fn rerun_failed_jobs_result() -> CodeHostResult {
    CodeHostResult::RerunFailedJobs(
        RerunFailedJobsResult::try_new(7001).expect("fixture rerun result is valid"),
    )
}

const REVIEW_SLOG_HEAD_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
const REVIEW_SLOG_BASE_REVISION: &str = "1111111111111111111111111111111111111111";
const REVIEW_SLOG_REVIEWED_AT: &str = "2026-07-27T10:00:00Z";
const REVIEW_SLOG_REPOSITORY: &str = "owner/repository";
const REVIEW_SLOG_NUMBER: u32 = 17;
const REVIEW_SLOG_BASE_REF: &str = "main";
const REVIEW_SLOG_HEAD_REF: &str = "feature";

fn reviewer_verdict_result() -> ReviewerVerdictEvidence {
    ReviewerVerdictEvidence::try_new(
        code_host_bounds(),
        ReviewerVerdictFields {
            status: ReviewerVerdictStatus::CurrentHead,
            reviewed_revision: Some(String::from(REVIEW_SLOG_HEAD_REVISION)),
            reviewed_at: Some(String::from(REVIEW_SLOG_REVIEWED_AT)),
            starvation_after_verdict: false,
            latest_starvation_at: None,
            latest_review_request_at: None,
            review_request_in_flight: false,
            source_truncated: false,
            comments_previous_cursor: None,
            reviews_previous_cursor: None,
        },
    )
    .expect("fixture reviewer verdict is bounded")
}

fn convergence_state_evidence() -> ConvergenceStateResult {
    ConvergenceStateResult::try_new(
        code_host_bounds(),
        ConvergenceStateFields {
            head_revision: String::from(REVIEW_SLOG_HEAD_REVISION),
            mergeable_state: String::from("MERGEABLE"),
            ci_rollup_state: Some(String::from("SUCCESS")),
            checks: Vec::new(),
            checks_truncated: false,
            checks_next_cursor: None,
            unresolved_threads: Vec::new(),
            open_escalations: Vec::new(),
            buried_escalations: Vec::new(),
            undispositioned_threads: Vec::new(),
            threads_truncated: false,
            threads_next_cursor: None,
            reviewer: reviewer_verdict_result(),
        },
    )
    .expect("fixture convergence state is bounded")
}

fn convergence_state_result() -> CodeHostResult {
    CodeHostResult::ConvergenceState(convergence_state_evidence())
}

fn stack_state_evidence() -> StackStateResult {
    StackStateResult::try_new(
        code_host_bounds(),
        StackStateFields {
            number: REVIEW_SLOG_NUMBER,
            base_ref: String::from(REVIEW_SLOG_BASE_REF),
            base_revision: String::from(REVIEW_SLOG_BASE_REVISION),
            head_ref: String::from(REVIEW_SLOG_HEAD_REF),
            head_revision: String::from(REVIEW_SLOG_HEAD_REVISION),
            default_ref: String::from(REVIEW_SLOG_BASE_REF),
            default_revision: String::from(REVIEW_SLOG_BASE_REVISION),
            base_commits_not_in_head: 0,
            main_commits_not_in_base: 0,
            children: Vec::new(),
            children_truncated: false,
            children_next_cursor: None,
        },
    )
    .expect("fixture stack state is bounded")
}

fn stack_state_result() -> CodeHostResult {
    CodeHostResult::StackState(stack_state_evidence())
}

fn thread_inventory_evidence() -> ThreadInventoryResult {
    let thread = ReviewThreadInventoryItem::try_new(
        code_host_bounds(),
        ReviewThreadInventoryFields {
            id: String::from("PRRT_thread"),
            path: String::from("src/lib.rs"),
            line: Some(12),
            resolved: true,
            outdated: false,
            author: Some(String::from("review-bot")),
            author_class: ReviewAuthorClass::Bot,
            finding_title: String::from("Finding title"),
            disposition: ReviewDispositionClass::FixNamed,
        },
    )
    .expect("fixture inventory item is bounded");
    ThreadInventoryResult::try_new(
        code_host_bounds(),
        String::from(REVIEW_SLOG_HEAD_REVISION),
        vec![thread],
        false,
        None,
    )
    .expect("fixture thread inventory is bounded")
}

fn thread_inventory_result() -> CodeHostResult {
    CodeHostResult::ThreadInventory(thread_inventory_evidence())
}

fn review_gate_result() -> CodeHostResult {
    let convergence = convergence_state_evidence();
    let stack = stack_state_evidence();
    let inventory = thread_inventory_evidence();
    CodeHostResult::ReviewGateCheck(ReviewGateCheckResult::compose(
        ReviewGatePurpose::DeclareConvergence,
        &convergence,
        &stack,
        &inventory,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnusedSessionStatusWriterError;

impl fmt::Display for UnusedSessionStatusWriterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unused session status writer was invoked")
    }
}

impl Error for UnusedSessionStatusWriterError {}

impl ClassifyOperatorFailure for UnusedSessionStatusWriterError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

#[derive(Clone, Copy, Debug)]
struct UnusedSessionStatusWriter;

impl SessionStatusWriter for UnusedSessionStatusWriter {
    type Error = UnusedSessionStatusWriterError;

    async fn write(
        &mut self,
        _update: SessionStatusWrite,
    ) -> Result<SessionStatusWriteOutcome, Self::Error> {
        Err(UnusedSessionStatusWriterError)
    }
}

impl ToolExecutor for RecordingExecutor {
    type Error = FixtureExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        self.correlations
            .lock()
            .expect("fixture correlation lock is available")
            .push(invocation.correlation());
        self.events
            .lock()
            .expect("fixture event lock is available")
            .push(invocation.request().name().as_str().to_owned());
        self.arguments
            .lock()
            .expect("fixture argument lock is available")
            .push(invocation.request().arguments().as_str().to_owned());
        let name = invocation.request().name().as_str().to_owned();
        if let Some(shutdown) = &self.shutdown {
            shutdown.send_replace(true);
        }
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
        self.correlations
            .lock()
            .expect("fixture correlation lock is available")
            .push(invocation.correlation());
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

/// A delegated park with no judge call yet remains a resumable turn after
/// composition restart and is judged exactly once by the fresh composition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegated_park_resumes_into_fresh_judge_composition() -> Result<(), Box<dyn Error>> {
    const TOOL_NAME: &str = "delegated";
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([delegated_tool(TOOL_NAME, ToolEffectClass::EffectFree)]);
    let executor = RecordingExecutor::completing();
    let (first_execution, _first_runtime) = fixture.execution(
        [tool_use_script(&[(TOOL_NAME, "{}")])],
        tool_catalog.clone(),
        executor.clone(),
    );

    first_execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let (scheduled, _dispatch_starts, continuation) =
        PostgresEligibilitySweep::new(fixture.pool.clone())
            .find_sessions()
            .await?
            .into_parts();
    let resumable = PostgresToolLoopRepository::new(fixture.pool.clone())
        .find_resumable_turn(fixture.session)
        .await?;
    let (restarted_execution, _runtime, judge_runtime) = fixture.execution_with_judge(
        [completion_script("restarted delegated result observed")],
        approval_judge_script("approve", "The restarted request is bounded."),
        tool_catalog,
        executor.clone(),
    );

    assert!(!continuation);
    assert_eq!(scheduled, vec![fixture.session]);
    assert_eq!(resumable, Some(fixture.turn));
    restarted_execution.resume_active(fixture.session).await?;
    assert_eq!(executor.events(), vec![String::from(TOOL_NAME)]);
    assert_eq!(judge_runtime.received_operations().len(), 1);
    Ok(())
}

/// Delegated approval parks first, records the deciding model and rationale,
/// executes exactly once, and exposes the dedicated judge in model-call
/// history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegated_approve_records_provenance_then_executes() -> Result<(), Box<dyn Error>> {
    const TOOL_NAME: &str = "delegated";
    const RECOMMENDATION: &str = "approve";
    const RATIONALE: &str = "The effect-free fixture request is bounded.";
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([delegated_tool(TOOL_NAME, ToolEffectClass::EffectFree)]);
    let executor = RecordingExecutor::completing();
    let (execution, _runtime, judge_runtime) = fixture.execution_with_judge(
        [
            tool_use_script(&[(TOOL_NAME, "{}")]),
            completion_script("delegated result observed"),
        ],
        approval_judge_script(RECOMMENDATION, RATIONALE),
        tool_catalog,
        executor.clone(),
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.request_ids().await?[0];
    let decision: DelegateDecisionProjection = sqlx::query_as(
        "SELECT decision.decision_kind, decision.decision_source,
                decision.rationale,
                judge.recommendation_kind AS recommendation,
                decision.delegate_model_call_id = judge.model_call_id AS model_call_matches,
                decision.delegate_model_selection_id AS model_selection_id
           FROM tool_approval_decision AS decision
           JOIN tool_approval_judge_model_call AS judge
             ON judge.request_id = decision.request_id
          WHERE decision.request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(executor.events(), vec![String::from(TOOL_NAME)]);
    assert_eq!(decision.decision_kind, RECOMMENDATION);
    assert_eq!(decision.decision_source, "delegate");
    assert_eq!(decision.rationale, RATIONALE);
    assert_eq!(decision.recommendation, RECOMMENDATION);
    assert!(decision.model_call_matches);
    assert_eq!(decision.model_selection_id, fixture.selection().into_uuid());
    assert_eq!(judge_runtime.received_operations().len(), 1);
    assert_eq!(
        model_call_history_count(&fixture.pool, fixture.session).await?,
        3
    );
    Ok(())
}

/// Delegated denial records the same full provenance, skips the executor, and
/// still advances through the ordinary continuation model call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegated_deny_records_provenance_then_skips_execution() -> Result<(), Box<dyn Error>> {
    const TOOL_NAME: &str = "delegated";
    const RECOMMENDATION: &str = "deny";
    const RATIONALE: &str = "The requested action is unnecessary.";
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([delegated_tool(TOOL_NAME, ToolEffectClass::EffectFree)]);
    let executor = RecordingExecutor::completing();
    let (execution, runtime, judge_runtime) = fixture.execution_with_judge(
        [
            tool_use_script(&[(TOOL_NAME, "{}")]),
            completion_script("delegated denial observed"),
        ],
        approval_judge_script(RECOMMENDATION, RATIONALE),
        tool_catalog,
        executor.clone(),
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.request_ids().await?[0];
    let decision: DelegateDecisionProjection = sqlx::query_as(
        "SELECT decision.decision_kind, decision.decision_source,
                decision.rationale,
                judge.recommendation_kind AS recommendation,
                decision.delegate_model_call_id = judge.model_call_id AS model_call_matches,
                decision.delegate_model_selection_id AS model_selection_id
           FROM tool_approval_decision AS decision
           JOIN tool_approval_judge_model_call AS judge
             ON judge.request_id = decision.request_id
          WHERE decision.request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert!(executor.events().is_empty());
    assert_eq!(decision.decision_kind, RECOMMENDATION);
    assert_eq!(decision.decision_source, "delegate");
    assert_eq!(decision.rationale, RATIONALE);
    assert_eq!(decision.recommendation, RECOMMENDATION);
    assert!(decision.model_call_matches);
    assert_eq!(decision.model_selection_id, fixture.selection().into_uuid());
    assert_eq!(runtime.received_operations().len(), 2);
    assert_eq!(judge_runtime.received_operations().len(), 1);
    assert_eq!(
        model_call_history_count(&fixture.pool, fixture.session).await?,
        3
    );
    Ok(())
}

/// Escalation records the judge call without fabricating a decision and keeps
/// the exact request parked until an explicit user command resolves it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegated_escalation_retains_park_for_user_resolution() -> Result<(), Box<dyn Error>> {
    const TOOL_NAME: &str = "delegated";
    const RECOMMENDATION: &str = "escalate_to_human";
    const RATIONALE: &str = "Human context is required.";
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([delegated_tool(TOOL_NAME, ToolEffectClass::EffectFree)]);
    let executor = RecordingExecutor::completing();
    let (execution, _runtime, judge_runtime) = fixture.execution_with_judge(
        [
            tool_use_script(&[(TOOL_NAME, "{}")]),
            completion_script("human-approved result observed"),
        ],
        approval_judge_script(RECOMMENDATION, RATIONALE),
        tool_catalog,
        executor.clone(),
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.request_ids().await?[0];
    let parked: EscalatedParkProjection = sqlx::query_as(
        "SELECT lifecycle.active_phase_kind AS active_phase,
                lifecycle.approval_tool_request_id AS approval_request_id,
                judge.recommendation_kind AS recommendation, judge.rationale,
                (SELECT count(*) FROM tool_approval_decision
                  WHERE request_id = judge.request_id) AS decision_count
           FROM turn_lifecycle AS lifecycle
           JOIN tool_approval_judge_model_call AS judge
             ON judge.session_id = lifecycle.session_id
            AND judge.turn_id = lifecycle.turn_id
          WHERE lifecycle.session_id = $1 AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(parked.active_phase, "awaiting_tool_approval");
    assert_eq!(parked.approval_request_id, request.into_uuid());
    assert_eq!(parked.recommendation, RECOMMENDATION);
    assert_eq!(parked.rationale, RATIONALE);
    assert_eq!(parked.decision_count, 0);
    assert!(executor.events().is_empty());
    assert_eq!(judge_runtime.received_operations().len(), 1);
    assert_eq!(
        model_call_history_count(&fixture.pool, fixture.session).await?,
        2
    );

    fixture
        .decide(request, ToolApprovalDecision::Approve)
        .await?;
    execution.resume_active(fixture.session).await?;

    assert_eq!(executor.events(), vec![String::from(TOOL_NAME)]);
    assert_eq!(
        model_call_history_count(&fixture.pool, fixture.session).await?,
        3
    );
    Ok(())
}

/// A shutdown requested during an issued tool operation waits for its durable
/// result, checkpoints there, and lets a successor finish without repeating
/// the tool or beginning an extra model call before restart.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn shutdown_checkpoints_after_the_issued_tool_before_the_next_model_call()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "checkpointed",
        ToolPermissionDefault::Auto,
        ToolEffectClass::EffectFree,
    )]);
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let executor = RecordingExecutor::completing_and_requesting_shutdown(shutdown_sender);
    let (execution, first_runtime) = fixture.execution(
        [
            tool_use_script(&[("checkpointed", "{}")]),
            completion_script("must remain unused before restart"),
        ],
        tool_catalog.clone(),
        executor.clone(),
    );

    execution
        .with_shutdown_checkpoint(shutdown_receiver)
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let active: (String, String) = sqlx::query_as(
        "SELECT state_kind, active_phase_kind
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert_eq!(executor.events(), vec![String::from("checkpointed")]);
    assert_eq!(first_runtime.received_operations().len(), 1);
    assert_eq!(
        model_call_history_count(&fixture.pool, fixture.session).await?,
        1
    );
    assert_eq!(active, (String::from("active"), String::from("running")));

    let resumed_executor = RecordingExecutor::completing();
    let (resumed, second_runtime) = fixture.execution(
        [completion_script("resumed after the shutdown checkpoint")],
        tool_catalog,
        resumed_executor.clone(),
    );
    resumed.resume_active(fixture.session).await?;

    assert!(resumed_executor.events().is_empty());
    assert_eq!(second_runtime.received_operations().len(), 1);
    assert_eq!(
        model_call_history_count(&fixture.pool, fixture.session).await?,
        2
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
    Ok(())
}

/// A shutdown requested during an issued model operation waits for its durable
/// result, checkpoints there, and lets a successor execute the requested tool
/// without beginning that tool operation before restart.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn shutdown_checkpoints_after_the_issued_model_before_the_next_tool()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "checkpointed",
        ToolPermissionDefault::Auto,
        ToolEffectClass::EffectFree,
    )]);
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let executor = RecordingExecutor::completing();
    let (execution, first_runtime) = fixture.execution_requesting_shutdown(
        [tool_use_script(&[("checkpointed", "{}")])],
        tool_catalog.clone(),
        executor.clone(),
        shutdown_sender,
    );

    execution
        .with_shutdown_checkpoint(shutdown_receiver)
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let active: (String, String) = sqlx::query_as(
        "SELECT state_kind, active_phase_kind
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert!(executor.events().is_empty());
    assert_eq!(first_runtime.received_operations().len(), 1);
    assert_eq!(
        model_call_history_count(&fixture.pool, fixture.session).await?,
        1
    );
    assert_eq!(active, (String::from("active"), String::from("running")));

    let (resumed, second_runtime) = fixture.execution(
        [completion_script("resumed after the shutdown checkpoint")],
        tool_catalog,
        executor.clone(),
    );
    resumed.resume_active(fixture.session).await?;

    assert_eq!(executor.events(), vec![String::from("checkpointed")]);
    assert_eq!(second_runtime.received_operations().len(), 1);
    assert_eq!(
        model_call_history_count(&fixture.pool, fixture.session).await?,
        2
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
    Ok(())
}

/// A shutdown requested between two operations checkpoints at the committed
/// attempt boundary the loop has just reached, issuing neither the tool
/// operation that boundary prepared nor the continuation provider round beyond
/// it, and lets a successor finish both.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn shutdown_checkpoints_at_a_committed_boundary_before_the_next_operation()
-> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let tool_catalog = ShutdownOnToolResolutionCatalog::new(
        catalog([tool(
            "checkpointed",
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        )]),
        shutdown_sender,
    );
    let executor = RecordingExecutor::completing();
    let (execution, first_runtime) = fixture.execution(
        [
            tool_use_script(&[("checkpointed", "{}")]),
            completion_script("must remain unused before restart"),
        ],
        tool_catalog.clone(),
        executor.clone(),
    );

    execution
        .with_shutdown_checkpoint(shutdown_receiver)
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let active: (String, String) = sqlx::query_as(
        "SELECT state_kind, active_phase_kind
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;

    assert!(executor.events().is_empty());
    assert_eq!(first_runtime.received_operations().len(), 1);
    assert_eq!(
        model_call_history_count(&fixture.pool, fixture.session).await?,
        1
    );
    assert_eq!(active, (String::from("active"), String::from("running")));

    let (resumed, second_runtime) = fixture.execution(
        [completion_script("resumed after the shutdown checkpoint")],
        tool_catalog,
        executor.clone(),
    );
    resumed.resume_active(fixture.session).await?;

    assert_eq!(executor.events(), vec![String::from("checkpointed")]);
    assert_eq!(second_runtime.received_operations().len(), 1);
    assert_eq!(
        model_call_history_count(&fixture.pool, fixture.session).await?,
        2
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
    Ok(())
}

/// The daemon's required automatic tool-round policy reaches the production
/// PostgreSQL tool loop and terminalizes before a disallowed provider call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn configured_automatic_tool_round_limit_stops_before_the_next_provider_call()
-> Result<(), Box<dyn Error>> {
    const TOOL_NAME: &str = "effect_free";
    const AUTOMATIC_TOOL_ROUND_LIMIT: usize = 1;
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let executor = RecordingExecutor::completing();
    let (execution, runtime) = fixture.execution_with_tool_round_limit(
        [
            tool_use_script(&[(TOOL_NAME, "{}")]),
            completion_script("must not be observed"),
        ],
        catalog([tool(
            TOOL_NAME,
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        )]),
        executor.clone(),
        Some(AUTOMATIC_TOOL_ROUND_LIMIT),
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;

    assert_eq!(executor.events(), vec![String::from(TOOL_NAME)]);
    assert_eq!(runtime.received_operations().len(), 1);
    assert_eq!(
        fixture.transcript_kinds().await?,
        vec![
            "origin_accepted_input",
            "assistant_tool_use",
            "tool_execution_result",
            "turn_failed",
        ]
    );
    Ok(())
}

/// S10:
/// one offline scripted turn parks for a user decision, executes exactly
/// once after approval with normalized arguments, commits a reference-only
/// result at the continuation boundary, and completes only after the second
/// model round.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_tool_loop_completes() -> Result<(), Box<dyn Error>> {
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
        continuation_tool_exchange(&runtime)?,
        vec![
            expected_tool_call(requests[0], "confirmed", r#"{"value":"one"}"#),
            expected_successful_tool_result(requests[0], String::from("completed:confirmed")),
        ]
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
    let identity_shape: (Uuid, Uuid, Uuid, Uuid, Uuid, Uuid, i64) = sqlx::query_as(
        "SELECT request.request_id,
                producing.turn_attempt_id,
                attempt.attempt_id,
                attempt.request_id,
                attempt.turn_id,
                attempt.issuing_turn_attempt_id,
                attempt.dispatch_generation::bigint
           FROM tool_request AS request
           JOIN model_call AS producing
             ON producing.model_call_id = request.producing_model_call_id
            AND producing.turn_id = request.turn_id
            AND producing.session_id = request.session_id
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
    assert_ne!(
        identity_shape.1, identity_shape.5,
        "the yielded tool-round attempt must differ from the producing model call's attempt"
    );
    assert_ne!(identity_shape.0, identity_shape.2);
    assert_eq!(identity_shape.3, identity_shape.0);
    assert_eq!(identity_shape.4, fixture.turn.into_uuid());
    assert_ne!(identity_shape.5, identity_shape.0);
    assert_ne!(identity_shape.5, identity_shape.2);
    assert_eq!(identity_shape.6, 1);
    let correlations = executor.correlations();
    let [correlation] = correlations.as_slice() else {
        panic!("exactly one dispatch fence must cross the executor boundary")
    };
    assert_eq!(correlation.session(), fixture.session);
    assert_eq!(correlation.turn(), fixture.turn);
    assert_eq!(correlation.issuing_attempt().into_uuid(), identity_shape.5);
    assert_eq!(correlation.request(), requests[0]);
    assert_eq!(correlation.attempt().into_uuid(), identity_shape.2);
    assert_eq!(correlation.generation(), ToolDispatchGeneration::first());
    Ok(())
}

/// The compiled echo declaration, typed decoder, daemon dispatcher, durable
/// tool loop, and continuation projection complete without network access.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_zero_echo_completes_offline_tool_loop() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let web = OfflineWebTransport::unused();
    let echoed_text = "offline echo";
    let arguments = serde_json::json!({"text": echoed_text}).to_string();
    let (tool_catalog, tool_executor) = offline_daemon_tools(
        web.clone(),
        UnusedSessionStatusWriter,
        UnusedCodeHostTransport,
        WebFetchEgressPolicy::deny_all(),
    )?
    .into_parts();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[("echo", arguments.as_str())]),
            completion_script("echo observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.wait_for_requests(1).await?[0];

    assert_eq!(
        continuation_tool_exchange(&runtime)?,
        vec![
            expected_tool_call(request, "echo", &arguments),
            expected_successful_tool_result(request, arguments),
        ]
    );
    assert!(
        web.requests().is_empty(),
        "echo must not enter the web transport"
    );
    Ok(())
}

/// The compiled web-fetch declaration and daemon dispatcher use only the
/// injected bounded transport while the durable tool loop remains fully
/// offline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_zero_web_fetch_completes_offline_tool_loop() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let expected_status = 200;
    let expected_content_type = "text/plain";
    let expected_body = "offline body";
    let response = WebFetchResponse::new(
        expected_status,
        Some(String::from(expected_content_type)),
        expected_body.as_bytes().to_vec(),
        WebFetchBodyCompleteness::Complete,
    )
    .expect("fixture response is bounded");
    let web = OfflineWebTransport::responding(response);
    let expected_url = "https://example.com/offline";
    let (tool_catalog, tool_executor) = offline_daemon_tools(
        web.clone(),
        UnusedSessionStatusWriter,
        UnusedCodeHostTransport,
        WebFetchEgressPolicy::try_from_allowed_origins([String::from("https://example.com")])?,
    )?
    .into_parts();
    let arguments = serde_json::json!({"url": expected_url}).to_string();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(WEB_FETCH_NAME, arguments.as_str())]),
            completion_script("fetch observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.wait_for_requests(1).await?[0];
    fixture
        .decide(request, ToolApprovalDecision::Approve)
        .await?;
    execution.resume_active(fixture.session).await?;
    let fetched = web.requests();
    let [physical_request] = fetched.as_slice() else {
        panic!("one physical fetch crosses the injected transport")
    };

    let expected_result = serde_json::json!({
        "body": expected_body,
        "content_type": expected_content_type,
        "status": expected_status,
        "truncated": false,
        "url": expected_url,
    })
    .to_string();

    assert_eq!(physical_request.url().as_str(), expected_url);
    assert_eq!(
        continuation_tool_exchange(&runtime)?,
        vec![
            expected_tool_call(request, WEB_FETCH_NAME, &arguments),
            expected_successful_tool_result(request, expected_result),
        ]
    );
    Ok(())
}

/// The confirmed session-status tool replaces the existing metadata snapshot
/// through the application service and attributes the durable write to the
/// exact tool request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_zero_session_status_updates_metadata_offline() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let web = OfflineWebTransport::unused();
    let expected_title = "Tool batch";
    let expected_tag = "tooling";
    let expected_attribute_key = "phase";
    let expected_attribute_value = "review";
    let expected_archived = false;
    let arguments = serde_json::json!({
        "archived": expected_archived,
        "attributes": {(expected_attribute_key): expected_attribute_value},
        "tags": [expected_tag],
        "title": expected_title,
    })
    .to_string();
    let (tool_catalog, tool_executor) = offline_daemon_tools(
        web.clone(),
        PostgresSessionStatusWriter::new(fixture.pool.clone()),
        UnusedCodeHostTransport,
        WebFetchEgressPolicy::deny_all(),
    )?
    .into_parts();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[("session_status_update", arguments.as_str())]),
            completion_script("status observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.wait_for_requests(1).await?[0];
    fixture
        .decide(request, ToolApprovalDecision::Approve)
        .await?;
    execution.resume_active(fixture.session).await?;
    let root: SessionMetadataRootFacts = sqlx::query_as(
        "SELECT title, archived, actor_kind, actor_tool_request_id
           FROM session_metadata
          WHERE session_id = $1",
    )
    .bind(fixture.session.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    let tags: Vec<String> = sqlx::query_scalar(
        "SELECT tag
           FROM session_metadata_tag
          WHERE session_id = $1
          ORDER BY tag",
    )
    .bind(fixture.session.into_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    let attributes: Vec<(String, String)> = sqlx::query_as(
        "SELECT attribute_key, attribute_value
           FROM session_metadata_attribute
          WHERE session_id = $1
          ORDER BY attribute_key",
    )
    .bind(fixture.session.into_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    let result = serde_json::json!({
        "archived": expected_archived,
        "attributes": {(expected_attribute_key): expected_attribute_value},
        "session_id": fixture.session.into_uuid().to_string(),
        "tags": [expected_tag],
        "title": expected_title,
    })
    .to_string();

    assert_eq!(
        root,
        SessionMetadataRootFacts {
            title: String::from(expected_title),
            archived: expected_archived,
            actor_kind: String::from("tool"),
            actor_tool_request_id: Some(request.into_uuid()),
        }
    );
    assert_eq!(tags, vec![String::from(expected_tag)]);
    assert_eq!(
        attributes,
        vec![(
            String::from(expected_attribute_key),
            String::from(expected_attribute_value),
        )]
    );
    assert_eq!(
        continuation_tool_exchange(&runtime)?,
        vec![
            expected_tool_call(request, "session_status_update", &arguments),
            expected_successful_tool_result(request, result),
        ]
    );
    assert!(
        web.requests().is_empty(),
        "session status must not enter the web transport"
    );
    Ok(())
}

/// S10: the composed GitHub metadata read is catalog-visible and crosses only
/// the injected credential, egress policy, and hermetic transport.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_composed_github_read_executes_offline() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let workspace = tempdir()?;
    let expected = serde_json::json!({"number": 17, "title": "offline pull request"});
    let github = RecordingGitHubTransport::responding(GitHubResult::metadata(expected.clone()));
    let (tool_catalog, tool_executor) = commissioned_daemon_tools(
        &fixture.pool,
        UnusedCodeHostTransport,
        github.clone(),
        workspace.path(),
    )?
    .into_parts();
    let expected_catalog_names = commissioned_catalog_names();
    let arguments = serde_json::json!({
        "repository": "KeenWill/signalbox",
        "number": 17
    })
    .to_string();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(PULL_REQUEST_METADATA_NAME, arguments.as_str())]),
            completion_script("GitHub metadata observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;

    assert_eq!(
        github.operations()[0].tool_name(),
        PULL_REQUEST_METADATA_NAME
    );
    assert_eq!(github.credential_matches(), vec![true]);
    assert_eq!(github.policy_matches(), vec![true]);
    assert_eq!(continuation_result_json(&runtime)?, expected);
    assert_commissioned_catalog(&runtime.received_operations()[0], &expected_catalog_names);
    Ok(())
}

/// S10: the composed workspace read is rooted in the injected temporary
/// directory and returns its exact fixture content without network access.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_composed_workspace_read_executes_offline() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let workspace = tempdir()?;
    let relative_path = "note.txt";
    let fixture_content = "workspace fixture\n";
    fs::write(workspace.path().join(relative_path), fixture_content)?;
    let (tool_catalog, tool_executor) = commissioned_daemon_tools(
        &fixture.pool,
        UnusedCodeHostTransport,
        UnusedGitHubTransport,
        workspace.path(),
    )?
    .into_parts();
    let expected_catalog_names = commissioned_catalog_names();
    let arguments = serde_json::json!({"path": relative_path, "max_bytes": 1024}).to_string();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(READ_FILE_NAME, arguments.as_str())]),
            completion_script("workspace content observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;

    assert_eq!(
        continuation_result_json(&runtime)?,
        serde_json::json!({
            "path": relative_path,
            "content": fixture_content,
            "offset": 0,
            "bytes_read": fixture_content.len(),
            "next_offset": fixture_content.len(),
            "total_bytes": fixture_content.len(),
            "truncated": false
        })
    );
    assert_commissioned_catalog(&runtime.received_operations()[0], &expected_catalog_names);
    Ok(())
}

/// S10: the composed local Git executor observes the injected repository
/// worktree and returns its fixture path through the daemon tool loop.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_composed_local_git_status_executes_offline() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let workspace = tempdir()?;
    let relative_path = "untracked.txt";
    let fixture_content = "local Git fixture\n";
    fs::write(workspace.path().join(relative_path), fixture_content)?;
    let (tool_catalog, tool_executor) = commissioned_daemon_tools(
        &fixture.pool,
        UnusedCodeHostTransport,
        UnusedGitHubTransport,
        workspace.path(),
    )?
    .into_parts();
    let expected_catalog_names = commissioned_catalog_names();
    let arguments = serde_json::json!({}).to_string();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(GIT_STATUS_NAME, arguments.as_str())]),
            completion_script("local Git status observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;

    let result = continuation_result_json(&runtime)?;
    assert_eq!(result["entries"][0]["path"], relative_path);
    assert_commissioned_catalog(&runtime.received_operations()[0], &expected_catalog_names);
    Ok(())
}

/// S10: the composed sandboxed executor reaches the injected process boundary
/// and returns its typed host-refusal evidence through the daemon tool loop.
/// The session blanket is enabled because `sandboxed_exec` declares `Confirm`,
/// so an unapproved proposal parks instead of dispatching and this test would
/// observe the approval gate rather than the process boundary it is about.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_composed_sandboxed_exec_executes_offline() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::ApproveAll).await?;
    let workspace = tempdir()?;
    let (tool_catalog, tool_executor) = commissioned_daemon_tools(
        &fixture.pool,
        UnusedCodeHostTransport,
        UnusedGitHubTransport,
        workspace.path(),
    )?
    .into_parts();
    let expected_catalog_names = commissioned_catalog_names();
    let arguments = serde_json::json!({"program": "cargo"}).to_string();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(SANDBOXED_EXEC_NAME, arguments.as_str())]),
            completion_script("sandbox host refusal observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;

    let result = continuation_result_json(&runtime)?;
    assert_eq!(
        result["confinement"],
        serde_json::json!({"kind": "sandbox_refused", "availability": "unusable"})
    );
    assert_eq!(
        result["outcome"],
        serde_json::json!({"kind": "spawn_failed", "reason": "sandbox_unavailable"})
    );
    assert_commissioned_catalog(&runtime.received_operations()[0], &expected_catalog_names);
    Ok(())
}

/// S10: the composed conversation port reads the invoking session's real
/// persisted semantic transcript rather than a synthetic transcript value.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_composed_introspection_returns_real_own_transcript() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let workspace = tempdir()?;
    let (tool_catalog, tool_executor) = commissioned_daemon_tools(
        &fixture.pool,
        UnusedCodeHostTransport,
        UnusedGitHubTransport,
        workspace.path(),
    )?
    .into_parts();
    let expected_catalog_names = commissioned_catalog_names();
    let arguments = serde_json::json!({
        "after_position": null,
        "max_entries": 100,
        "max_bytes": 131072
    })
    .to_string();
    let expected_user_content = format!(r#"[{{"type":"text","text":"{FIXTURE_USER_CONTENT}"}}]"#);
    let expected_tool_use_content = format!(
        "{}\n{arguments}",
        signalbox_tools_conversations::READ_OWN_CONVERSATION_NAME
    );
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(
                signalbox_tools_conversations::READ_OWN_CONVERSATION_NAME,
                arguments.as_str(),
            )]),
            completion_script("own transcript observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    assert_eq!(
        continuation_result_json(&runtime)?,
        serde_json::json!({
            "session_id": fixture.session.into_uuid().to_string(),
            "entries": [{
                "position": 1,
                "kind": "user",
                "content": expected_user_content,
                "content_truncated": false
            }, {
                "position": 2,
                "kind": "tool_use",
                "content": expected_tool_use_content,
                "content_truncated": false
            }],
            "next_after": null,
            "truncated": false
        })
    );
    assert_commissioned_catalog(&runtime.received_operations()[0], &expected_catalog_names);
    Ok(())
}

/// S10: workspace mutation remains parked with no filesystem effect until a
/// user approval is recorded through the process protocol.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s10_workspace_write_gates_through_process_protocol() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let workspace = tempdir()?;
    let relative_path = "approved.txt";
    let destination = workspace.path().join(relative_path);
    let expected_content = "approved workspace write";
    let (tool_catalog, tool_executor) = commissioned_daemon_tools(
        &fixture.pool,
        UnusedCodeHostTransport,
        UnusedGitHubTransport,
        workspace.path(),
    )?
    .into_parts();
    let arguments = serde_json::json!({
        "path": relative_path,
        "content": expected_content
    })
    .to_string();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(WRITE_FILE_NAME, arguments.as_str())]),
            completion_script("workspace write observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.wait_for_requests(1).await?[0];

    assert!(!destination.exists());
    let receipt = approve_through_process(&fixture, request, 0x3c02).await?;
    assert_approved_receipt(receipt, request);
    assert!(!destination.exists());
    execution.resume_active(fixture.session).await?;
    assert_eq!(fs::read_to_string(&destination)?, expected_content);
    assert_eq!(
        continuation_result_json(&runtime)?,
        serde_json::json!({
            "path": relative_path,
            "bytes_written": expected_content.len(),
            "created": true
        })
    );
    Ok(())
}

/// S10: review publication remains parked with no transport effect until a
/// user approval is recorded through the process protocol.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s10_github_publish_gates_through_process_protocol() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let workspace = tempdir()?;
    let expected = serde_json::json!({"review_id": 91, "state": "APPROVED"});
    let github =
        RecordingGitHubTransport::responding(GitHubResult::published_review(expected.clone()));
    let (tool_catalog, tool_executor) = commissioned_daemon_tools(
        &fixture.pool,
        UnusedCodeHostTransport,
        github.clone(),
        workspace.path(),
    )?
    .into_parts();
    let arguments = serde_json::json!({
        "repository": "KeenWill/signalbox",
        "number": 17,
        "commit_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "event": "approve",
        "comments": []
    })
    .to_string();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[(PULL_REQUEST_PUBLISH_REVIEW_NAME, arguments.as_str())]),
            completion_script("published review observed"),
        ],
        tool_catalog,
        tool_executor,
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.wait_for_requests(1).await?[0];

    assert!(github.operations().is_empty());
    let receipt = approve_through_process(&fixture, request, 0x3c03).await?;
    assert_approved_receipt(receipt, request);
    assert!(github.operations().is_empty());
    execution.resume_active(fixture.session).await?;
    assert_eq!(
        github.operations()[0].tool_name(),
        PULL_REQUEST_PUBLISH_REVIEW_NAME
    );
    assert_eq!(github.credential_matches(), vec![true]);
    assert_eq!(github.policy_matches(), vec![true]);
    assert_eq!(continuation_result_json(&runtime)?, expected);
    Ok(())
}

/// Tier 1: summary lookup crosses the typed mock, scrubs reflected
/// credential text, and persists only the bounded result in the continuation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_summary_completes_offline_tool_loop() -> Result<(), Box<dyn Error>>
{
    code_host_tool_completes_offline(
        CHANGE_REQUEST_SUMMARY_NAME,
        serde_json::json!({"number": 17, "repository": "owner/repository"}).to_string(),
        summary_result(),
        serde_json::json!({
            "author": "reviewer",
            "base_ref": "main",
            "body": "bounded body",
            "draft": false,
            "head_ref": "feature",
            "head_revision": "0123456789abcdef0123456789abcdef01234567",
            "number": 17,
            "state": "open",
            "title": "summary [redacted]",
            "url": "https://github.example/owner/repository/pull/17",
        }),
        ExpectedCodeHostOperation::Summary {
            repository: "owner/repository",
            number: 17,
        },
        ExpectedCodeHostApproval::Auto,
    )
    .await
}

/// Tier 1 changed-files lookup crosses only the typed mocked transport.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_changed_files_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_CHANGED_FILES_NAME,
        serde_json::json!({"number": 17, "repository": "owner/repository"}).to_string(),
        changed_files_result(),
        serde_json::json!({
            "files": [{
                "additions": 7,
                "deletions": 2,
                "path": "src/lib.rs",
                "status": "modified",
            }],
            "truncated": false,
        }),
        ExpectedCodeHostOperation::ChangedFiles {
            repository: "owner/repository",
            number: 17,
        },
        ExpectedCodeHostApproval::Auto,
    )
    .await
}

/// Tier 1 per-file patch lookup preserves the exact checked path offline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_file_patch_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_FILE_PATCH_NAME,
        serde_json::json!({
            "number": 17,
            "path": "src/lib.rs",
            "repository": "owner/repository",
        })
        .to_string(),
        file_patch_result(),
        serde_json::json!({
            "file": {
                "additions": 7,
                "deletions": 2,
                "path": "src/lib.rs",
                "status": "modified",
            },
            "patch": "@@ -1 +1 @@\n-old\n+new",
        }),
        ExpectedCodeHostOperation::FilePatch {
            repository: "owner/repository",
            number: 17,
            path: "src/lib.rs",
        },
        ExpectedCodeHostApproval::Auto,
    )
    .await
}

/// Tier 1 checks lookup preserves the frozen revision offline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_checks_status_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_CHECKS_STATUS_NAME,
        serde_json::json!({
            "repository": "owner/repository",
            "revision": "0123456789abcdef0123456789abcdef01234567",
        })
        .to_string(),
        checks_status_result(),
        serde_json::json!({
            "checks": [{
                "conclusion": "success",
                "id": 9001,
                "name": "validate",
                "status": "completed",
                "url": "https://github.example/check/9001",
            }],
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "truncated": false,
        }),
        ExpectedCodeHostOperation::ChecksStatus {
            repository: "owner/repository",
            revision: "0123456789abcdef0123456789abcdef01234567",
        },
        ExpectedCodeHostApproval::Auto,
    )
    .await
}

/// Tier 1 top-level comment creation remains parked until user approval and
/// then crosses only the typed mocked transport.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_comment_completes_offline_tool_loop() -> Result<(), Box<dyn Error>>
{
    code_host_tool_completes_offline(
        CHANGE_REQUEST_COMMENT_NAME,
        serde_json::json!({
            "body": "offline review",
            "number": 17,
            "repository": "owner/repository",
        })
        .to_string(),
        comment_result(),
        serde_json::json!({"id": 8001, "url": "https://github.example/comment/8001"}),
        ExpectedCodeHostOperation::Comment {
            repository: "owner/repository",
            number: 17,
            body: "offline review",
        },
        ExpectedCodeHostApproval::Confirm,
    )
    .await
}

/// Tier 1 review-thread lookup crosses only the typed mocked transport.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_review_threads_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_REVIEW_THREADS_NAME,
        serde_json::json!({"number": 17, "repository": "owner/repository"}).to_string(),
        review_threads_result(),
        serde_json::json!({
            "threads": [{
                "comments": [{
                    "author": "reviewer",
                    "body": "please adjust",
                    "id": "PRRC_comment",
                    "url": "https://github.example/comment/7001",
                }],
                "comments_truncated": false,
                "id": "PRRT_thread",
                "line": 12,
                "outdated": false,
                "path": "src/lib.rs",
                "resolved": false,
            }],
            "truncated": false,
        }),
        ExpectedCodeHostOperation::ReviewThreads {
            repository: "owner/repository",
            number: 17,
        },
        ExpectedCodeHostApproval::Auto,
    )
    .await
}

/// Tier 1 thread replies remain parked until user approval and preserve the
/// exact opaque thread identity offline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_thread_reply_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_THREAD_REPLY_NAME,
        serde_json::json!({
            "body": "fixed offline",
            "number": 17,
            "repository": "owner/repository",
            "thread_id": "PRRT_thread",
        })
        .to_string(),
        thread_reply_result(),
        serde_json::json!({"id": "PRRC_reply", "url": "https://github.example/comment/7002"}),
        ExpectedCodeHostOperation::ThreadReply {
            repository: "owner/repository",
            number: 17,
            thread_id: "PRRT_thread",
            body: "fixed offline",
        },
        ExpectedCodeHostApproval::Confirm,
    )
    .await
}

/// Tier 1 thread resolution remains parked until user approval and preserves
/// the exact opaque thread identity offline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_thread_resolve_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_THREAD_RESOLVE_NAME,
        serde_json::json!({
            "number": 17,
            "repository": "owner/repository",
            "thread_id": "PRRT_thread",
        })
        .to_string(),
        thread_resolve_result(),
        serde_json::json!({"resolved": true, "thread_id": "PRRT_thread"}),
        ExpectedCodeHostOperation::ThreadResolve {
            repository: "owner/repository",
            number: 17,
            thread_id: "PRRT_thread",
        },
        ExpectedCodeHostApproval::Confirm,
    )
    .await
}

/// Tier 1 CI job-log lookup preserves the exact job identity offline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_ci_job_log_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_CI_JOB_LOG_NAME,
        serde_json::json!({"job_id": 9001, "repository": "owner/repository"}).to_string(),
        ci_job_log_result(),
        serde_json::json!({"job_id": 9001, "text": "offline job log", "truncated": false}),
        ExpectedCodeHostOperation::CiJobLog {
            repository: "owner/repository",
            job_id: 9001,
        },
        ExpectedCodeHostApproval::Auto,
    )
    .await
}

/// Tier 1 failed-job reruns remain parked until user approval and preserve
/// the exact workflow-run identity offline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_rerun_failed_jobs_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME,
        serde_json::json!({"repository": "owner/repository", "run_id": 7001}).to_string(),
        rerun_failed_jobs_result(),
        serde_json::json!({"run_id": 7001}),
        ExpectedCodeHostOperation::RerunFailedJobs {
            repository: "owner/repository",
            run_id: 7001,
        },
        ExpectedCodeHostApproval::Confirm,
    )
    .await
}

/// Tier 1 convergence lookup crosses only the typed mocked transport and
/// persists its derived current-head verdict.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_convergence_state_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_CONVERGENCE_STATE_NAME,
        serde_json::json!({"number": REVIEW_SLOG_NUMBER, "repository": REVIEW_SLOG_REPOSITORY})
            .to_string(),
        convergence_state_result(),
        serde_json::json!({
            "buried_escalations": [],
            "checks": [],
            "checks_next_cursor": null,
            "checks_truncated": false,
            "ci_green": true,
            "ci_rollup_state": "SUCCESS",
            "head_revision": REVIEW_SLOG_HEAD_REVISION,
            "mergeable_state": "MERGEABLE",
            "open_escalations": [],
            "reviewer_verdict": {
                "comments_previous_cursor": null,
                "latest_starvation_at": null,
                "latest_review_request_at": null,
                "review_request_in_flight": false,
                "reviewed_at": REVIEW_SLOG_REVIEWED_AT,
                "reviewed_revision": REVIEW_SLOG_HEAD_REVISION,
                "reviews_previous_cursor": null,
                "source_truncated": false,
                "starvation_after_verdict": false,
                "status": "current_head",
            },
            "threads_next_cursor": null,
            "threads_truncated": false,
            "unresolved_thread_count": 0,
            "undispositioned_thread_count": 0,
            "undispositioned_threads": [],
            "unresolved_threads": [],
            "verdict": "converged",
        }),
        ExpectedCodeHostOperation::ConvergenceState {
            repository: REVIEW_SLOG_REPOSITORY,
            number: REVIEW_SLOG_NUMBER,
        },
        ExpectedCodeHostApproval::Auto,
    )
    .await
}

/// Tier 1 stack lookup preserves its explicit child-page continuation offline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_stack_state_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_STACK_STATE_NAME,
        serde_json::json!({
            "cursor": "opaque-child-page",
            "number": REVIEW_SLOG_NUMBER,
            "repository": REVIEW_SLOG_REPOSITORY,
        })
        .to_string(),
        stack_state_result(),
        serde_json::json!({
            "base_commits_not_in_head": 0,
            "base_needs_merge_forward": false,
            "base_ref": REVIEW_SLOG_BASE_REF,
            "base_revision": REVIEW_SLOG_BASE_REVISION,
            "children": [],
            "children_next_cursor": null,
            "children_truncated": false,
            "default_ref": REVIEW_SLOG_BASE_REF,
            "default_revision": REVIEW_SLOG_BASE_REVISION,
            "head_ref": REVIEW_SLOG_HEAD_REF,
            "head_revision": REVIEW_SLOG_HEAD_REVISION,
            "main_commits_not_in_base": 0,
            "main_missing_from_base_chain": false,
            "number": REVIEW_SLOG_NUMBER,
        }),
        ExpectedCodeHostOperation::StackState {
            repository: REVIEW_SLOG_REPOSITORY,
            number: REVIEW_SLOG_NUMBER,
            cursor: Some("opaque-child-page"),
        },
        ExpectedCodeHostApproval::Auto,
    )
    .await
}

/// Tier 1 thread inventory preserves its opaque continuation and structured
/// disposition evidence offline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_change_request_thread_inventory_completes_offline_tool_loop()
-> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        CHANGE_REQUEST_THREAD_INVENTORY_NAME,
        serde_json::json!({
            "cursor": "opaque-cursor",
            "number": REVIEW_SLOG_NUMBER,
            "repository": REVIEW_SLOG_REPOSITORY,
        })
        .to_string(),
        thread_inventory_result(),
        serde_json::json!({
            "next_cursor": null,
            "head_revision": REVIEW_SLOG_HEAD_REVISION,
            "threads": [{
                "author": "review-bot",
                "author_class": "bot",
                "disposition": "fix_named",
                "finding_title": "Finding title",
                "id": "PRRT_thread",
                "line": 12,
                "outdated": false,
                "path": "src/lib.rs",
                "resolved": true,
            }],
            "truncated": false,
        }),
        ExpectedCodeHostOperation::ThreadInventory {
            repository: REVIEW_SLOG_REPOSITORY,
            number: REVIEW_SLOG_NUMBER,
            cursor: Some("opaque-cursor"),
        },
        ExpectedCodeHostApproval::Auto,
    )
    .await
}

/// Tier 1 review gating persists the pure composition result while its fresh
/// evidence reads cross only the typed mocked transport boundary.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tier_one_review_gate_check_completes_offline_tool_loop() -> Result<(), Box<dyn Error>> {
    code_host_tool_completes_offline(
        REVIEW_GATE_CHECK_NAME,
        serde_json::json!({
            "number": REVIEW_SLOG_NUMBER,
            "purpose": "declare_convergence",
            "repository": REVIEW_SLOG_REPOSITORY,
        })
        .to_string(),
        review_gate_result(),
        serde_json::json!({
            "blockers": [],
            "head_revision": REVIEW_SLOG_HEAD_REVISION,
            "purpose": "declare_convergence",
            "ready": true,
        }),
        ExpectedCodeHostOperation::ReviewGateCheck {
            repository: REVIEW_SLOG_REPOSITORY,
            number: REVIEW_SLOG_NUMBER,
            purpose: ReviewGatePurpose::DeclareConvergence,
        },
        ExpectedCodeHostApproval::Auto,
    )
    .await
}

/// S10 / S11: user denial creates no physical
/// attempt, projects one error result to the continuation call, and allows the
/// same turn to complete from the model's response.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_s11_denial_continues_without_execution() -> Result<(), Box<dyn Error>> {
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
        continuation_tool_exchange(&runtime)?,
        vec![
            expected_tool_call(request, "confirmed", "{}"),
            expected_failed_tool_result(
                request,
                serde_json::json!({
                    "error": {
                        "kind": "denied",
                        "detail": null,
                    }
                })
                .to_string(),
            ),
        ]
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
        (String::from("deny"), String::from("user_command"), 0)
    );
    Ok(())
}

/// S10 / S11: deny-and-end first
/// records the exact denial, then the ordinary proof-bearing interrupt closes
/// the active turn; no tool attempt is created, the stop remains independently
/// auditable, and a later submit survives reconstitution before its new turn
/// activates and runs.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_s11_cancelled_tool_round_admits_and_runs_later_turn() -> Result<(), Box<dyn Error>> {
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
                descendant_scope: DescendantTerminationScope::ParentAlone,
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
    let terminal_shape: (String, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT terminal_disposition_kind,
                (SELECT count(*) FROM tool_attempt WHERE request_id = $3),
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE source_session_id = $1
                    AND payload_kind = 'tool_denied'
                    AND tool_result_request_id = $3),
                (SELECT count(*) FROM semantic_transcript_entry
                  WHERE source_session_id = $1
                    AND payload_kind = 'turn_cancelled'
                    AND cancelled_turn_id = $2),
                (SELECT count(*) FROM model_call
                  WHERE session_id = $1
                    AND turn_id = $2)
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(request.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(terminal_shape, (String::from("cancelled"), 0, 1, 1, 1));
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

    let later_turn = fixture
        .submit_new_turn(0x3312, "work after cancelled tool round")
        .await?;
    fixture
        .activate_and_complete_turn(origin.turn(), "interrupt successor completed")
        .await?;
    fixture
        .activate_and_complete_turn(later_turn, "post-cancellation submit completed")
        .await?;
    Ok(())
}

/// S07 / S10: an interrupt alone against a parked approval
/// wait records the authoritative typed rejection — it is not a denial and
/// does not bypass the decision command — and the wait remains parked with no
/// tool attempt until its canonical decision command resolves the obligation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s07_s10_interrupt_against_parked_approval_wait_is_rejected() -> Result<(), Box<dyn Error>>
{
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

    let sweep = PostgresEligibilitySweep::new(fixture.pool.clone());
    let (nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let mut submit = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        SubmitInputRepository::new(fixture.pool.clone()),
        nudge,
        fixture.tool_dispatch_gate.clone(),
    );
    let outcome = submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x3320)),
            fixture.session,
            UserContent::try_text(String::from("stop while confirm is pending"))
                .expect("fixture interrupt content is admitted"),
            DeliveryRequest::Interrupt {
                expected_active_turn: fixture.turn,
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: default_configuration(),
            },
        )?)
        .await?;
    assert_eq!(
        outcome,
        SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                session: fixture.session,
                active_turn: fixture.turn,
            },
        )),
        "an interrupt alone must not bypass the decision command"
    );

    assert!(executor.events().is_empty());
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
            Some(request.into_uuid()),
            0,
        ),
        "the approval wait must remain parked after the rejected interrupt"
    );
    Ok(())
}

/// S02 / S10: a restart scan preserves an
/// approval wait exactly; after the user decision, the durable sweep and a
/// fresh composition resume the same logical turn without replaying activation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s10_restart_leaves_approval_turn_parked() -> Result<(), Box<dyn Error>> {
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
    assert_eq!(outcome.recovered_turn_count(), 0);
    let repeated = scan.execute().await?;
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
    let (resumable, _dispatch_starts, continuation) =
        PostgresEligibilitySweep::new(fixture.pool.clone())
            .find_sessions()
            .await?
            .into_parts();
    assert!(!continuation);
    assert_eq!(resumable, vec![fixture.session]);
    let (restarted_execution, restarted_runtime) = fixture.execution(
        [completion_script("continued after restart")],
        tool_catalog,
        executor.clone(),
    );
    restarted_execution.resume_active(fixture.session).await?;
    assert_eq!(executor.events(), vec![String::from("confirmed")]);
    assert_eq!(
        continuation_tool_exchange(&restarted_runtime)?,
        vec![
            expected_tool_call(request, "confirmed", "{}"),
            expected_successful_tool_result(request, String::from("completed:confirmed")),
        ],
        "the fresh composition must reach the correlated continuation call"
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
    let terminal_shape: (String, i64) = sqlx::query_as(
        "SELECT terminal_disposition_kind,
                (SELECT count(*) FROM model_call
                  WHERE session_id = $1 AND turn_id = $2)
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(terminal_shape, (String::from("completed"), 2));
    Ok(())
}

/// S10: an auto/confirm batch parks on
/// its earliest undecided request and, after approval, executes both requests
/// serially in proposal order with their distinct provenance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_mixed_batch_executes_in_proposal_order() -> Result<(), Box<dyn Error>> {
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
    let (execution, runtime) = fixture.execution(
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
    let (resumed, observed_first) = tokio::join!(
        execution.resume_active(fixture.session),
        executor.assert_first_only_then_release("automatic")
    );
    observed_first?;
    resumed?;

    assert_eq!(
        executor.events(),
        vec![String::from("automatic"), String::from("confirmed")]
    );
    assert_eq!(
        continuation_tool_exchange(&runtime)?,
        vec![
            expected_tool_call(requests[0], "automatic", "{}"),
            expected_tool_call(requests[1], "confirmed", "{}"),
            expected_successful_tool_result(requests[0], String::from("completed:automatic")),
            expected_successful_tool_result(requests[1], String::from("completed:confirmed")),
        ],
        "continuation history retains paired calls and proposal-ordered results"
    );
    let correlations = executor.correlations();
    let [automatic, confirmed] = correlations.as_slice() else {
        panic!("each proposal must cross the executor boundary exactly once")
    };
    assert_eq!(automatic.request(), requests[0]);
    assert_eq!(confirmed.request(), requests[1]);
    assert_ne!(automatic.attempt(), confirmed.attempt());
    assert_eq!(automatic.session(), fixture.session);
    assert_eq!(confirmed.session(), fixture.session);
    assert_eq!(automatic.turn(), fixture.turn);
    assert_eq!(confirmed.turn(), fixture.turn);
    assert_eq!(automatic.issuing_attempt(), confirmed.issuing_attempt());
    assert_eq!(automatic.generation(), ToolDispatchGeneration::first());
    assert_eq!(confirmed.generation(), ToolDispatchGeneration::first());
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
                String::from("user_command"),
                String::from("completed"),
            ),
        ]
    );
    Ok(())
}

/// S10: the explicitly dangerous frozen blanket posture
/// approves a confirm-default tool under `session_blanket` provenance and the
/// turn runs unattended without fabricating user agency.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_blanket_posture_runs_confirm_tool_unattended() -> Result<(), Box<dyn Error>> {
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
        "SELECT approval.decision_source, approval.user_command_id,
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

/// S05: losing a dispatched effect-free attempt
/// never retries it; the dispatch path contains the executor failure by
/// classifying it `known_failed` with `crash_lost` evidence before releasing
/// its gate, startup preserves that terminal state idempotently, and a later
/// submit activates and runs.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s05_failed_tool_round_admits_and_runs_later_turn() -> Result<(), Box<dyn Error>> {
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
    first_execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    assert_eq!(crashing.events(), vec![String::from("effect_free")]);

    let mut startup = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(fixture.pool.clone()),
    );
    let recovery = startup.execute().await?;
    assert_eq!(
        recovery.recovered_turn_count(),
        0,
        "dispatch failure already classified the effect-free attempt and failed the turn"
    );
    let repeated_recovery = startup.execute().await?;
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
            "tool_execution_result",
            "turn_failed",
        ]
    );

    let later_turn = fixture
        .submit_new_turn(0x3412, "work after failed tool round")
        .await?;
    fixture
        .activate_and_complete_turn(later_turn, "post-failure submit completed")
        .await?;
    Ok(())
}

/// S02 / S10: an ordinary provider failure on the continuation model
/// call of a completed tool round terminalizes the turn naming that call, and
/// the committed terminal shape reloads through the scheduling projection —
/// the startup scan completes and the next submit activates and runs instead
/// of the session becoming permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s10_failed_continuation_call_admits_and_runs_later_turn() -> Result<(), Box<dyn Error>>
{
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "effect_free",
        ToolPermissionDefault::Auto,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[("effect_free", "{}")]),
            provider_error_script(),
        ],
        tool_catalog,
        executor.clone(),
    );
    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    assert_eq!(executor.events(), vec![String::from("effect_free")]);
    assert_eq!(
        runtime.received_operations().len(),
        2,
        "the failed continuation round follows the completed tool round"
    );
    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct FailedContinuationShape {
        turn_disposition: String,
        terminal_model_call_id: Option<Uuid>,
        call_disposition: String,
        provider_failure_cause: Option<String>,
        model_call_count: i64,
    }
    let terminal_shape: FailedContinuationShape = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind AS turn_disposition,
                lifecycle.terminal_model_call_id,
                continuation.terminal_disposition_kind AS call_disposition,
                continuation.terminal_provider_failure_cause AS provider_failure_cause,
                (SELECT count(*) FROM model_call
                  WHERE session_id = $1 AND turn_id = $2) AS model_call_count
           FROM turn_lifecycle AS lifecycle
           JOIN model_call AS continuation
             ON continuation.session_id = lifecycle.session_id
            AND continuation.model_call_id = lifecycle.terminal_model_call_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(terminal_shape.turn_disposition, "failed");
    assert!(
        terminal_shape.terminal_model_call_id.is_some(),
        "the failed turn names its continuation call"
    );
    assert_eq!(terminal_shape.call_disposition, "known_failed");
    assert_eq!(
        terminal_shape.provider_failure_cause,
        Some(String::from("provider_internal"))
    );
    assert_eq!(
        terminal_shape.model_call_count, 2,
        "the terminal call is the second-round continuation call"
    );
    assert_eq!(
        fixture.transcript_kinds().await?,
        vec![
            "origin_accepted_input",
            "assistant_tool_use",
            "tool_execution_result",
            "turn_failed",
        ]
    );

    let mut startup = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(fixture.pool.clone()),
    );
    let recovery = startup.execute().await?;
    assert_eq!(
        recovery.recovered_turn_count(),
        0,
        "the provider failure already terminalized the turn"
    );

    let later_turn = fixture
        .submit_new_turn(0x3512, "work after failed continuation call")
        .await?;
    fixture
        .activate_and_complete_turn(later_turn, "post-continuation-failure submit completed")
        .await?;
    Ok(())
}

async fn submit_frame_through_process(
    fixture: &ToolLoopFixture,
    frame: &ClientFrame,
) -> Result<ServerMessage, Box<dyn Error>> {
    let directory = tempdir()?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
    let socket = directory.path().join("hub.sock");
    let listener = LocalProcessListener::bind(&socket)?;
    let sweep = PostgresEligibilitySweep::new(fixture.pool.clone());
    let (eligibility_nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let runtime = ProcessRuntime::new(
        listener,
        fixture.pool.clone(),
        eligibility_nudge,
        fixture.tool_dispatch_gate.clone(),
        support::parse_model_configuration(PROCESS_MODEL_CONFIGURATION)?,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let runtime_task = tokio::spawn(runtime.run(shutdown_receiver));

    let stream = UnixStream::connect(&socket).await?;
    let (reader, mut writer) = stream.into_split();
    writer.write_all(&encode_client_line(frame)?).await?;
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).await?;
    let response = decode_server_line(&line)?.message().clone();

    drop(reader);
    drop(writer);
    shutdown.send(true)?;
    timeout(Duration::from_secs(10), runtime_task).await???;
    Ok(response)
}

async fn approve_through_process(
    fixture: &ToolLoopFixture,
    request: ToolRequestId,
    command_seed: u128,
) -> Result<ServerMessage, Box<dyn Error>> {
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        RequestId::try_new(1)?,
        ClientRequest::DecideToolRequest {
            command_id: CommandId::try_from_uuid(Uuid::from_u128(command_seed))?,
            session_id: CanonicalUuid::from_uuid(fixture.session.into_uuid()),
            tool_request_id: CanonicalUuid::from_uuid(request.into_uuid()),
            decision: ToolDecision::Approve {},
        },
    )?;
    submit_frame_through_process(fixture, &frame).await
}

#[track_caller]
fn assert_approved_receipt(message: ServerMessage, request: ToolRequestId) {
    assert_eq!(
        message,
        ServerMessage::ToolRequestDecided {
            tool_request_id: CanonicalUuid::from_uuid(request.into_uuid()),
            decision: ToolDecision::Approve {},
        }
    );
}

/// S02 / S10: a provider refusal on the continuation model call of
/// a completed tool round terminalizes the turn as refused naming that call,
/// and the committed refused shape reloads through the scheduling
/// projection — the startup scan completes and the next submit activates and
/// runs instead of the session becoming permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s10_refused_continuation_call_admits_and_runs_later_turn() -> Result<(), Box<dyn Error>>
{
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "effect_free",
        ToolPermissionDefault::Auto,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let (execution, runtime) = fixture.execution(
        [tool_use_script(&[("effect_free", "{}")]), refusal_script()],
        tool_catalog,
        executor.clone(),
    );
    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    assert_eq!(executor.events(), vec![String::from("effect_free")]);
    assert_eq!(
        runtime.received_operations().len(),
        2,
        "the refused continuation round follows the completed tool round"
    );
    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct RefusedContinuationShape {
        turn_disposition: String,
        terminal_model_call_id: Option<Uuid>,
        call_disposition: String,
        model_call_count: i64,
    }
    let terminal_shape: RefusedContinuationShape = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind AS turn_disposition,
                lifecycle.terminal_model_call_id,
                continuation.terminal_disposition_kind AS call_disposition,
                (SELECT count(*) FROM model_call
                  WHERE session_id = $1 AND turn_id = $2) AS model_call_count
           FROM turn_lifecycle AS lifecycle
           JOIN model_call AS continuation
             ON continuation.session_id = lifecycle.session_id
            AND continuation.model_call_id = lifecycle.terminal_model_call_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(terminal_shape.turn_disposition, "refused");
    assert!(
        terminal_shape.terminal_model_call_id.is_some(),
        "the refused turn names its continuation call"
    );
    assert_eq!(terminal_shape.call_disposition, "refused");
    assert_eq!(
        terminal_shape.model_call_count, 2,
        "the refusing call is the second-round continuation call"
    );
    assert_eq!(
        fixture.transcript_kinds().await?,
        vec![
            "origin_accepted_input",
            "assistant_tool_use",
            "tool_execution_result",
        ],
        "a refusal appends no semantic content after the round's results"
    );

    let mut startup = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(fixture.pool.clone()),
    );
    let recovery = startup.execute().await?;
    assert_eq!(
        recovery.recovered_turn_count(),
        0,
        "the provider refusal already terminalized the turn"
    );

    let later_turn = fixture
        .submit_new_turn(0x3812, "work after refused continuation call")
        .await?;
    fixture
        .activate_and_complete_turn(later_turn, "post-continuation-refusal submit completed")
        .await?;
    Ok(())
}

/// S02 / S08 / S10: a NextSafePoint input accepted through
/// while a tool round is parked is consumed by the
/// continuation call, the
/// steering-bearing continuation completes the turn, and the committed shape
/// reloads through the scheduling projection — the startup scan completes and
/// the next submit activates and runs.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s08_s10_steering_consumed_at_continuation_completes() -> Result<(), Box<dyn Error>> {
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
            completion_script("steered tool result observed"),
        ],
        tool_catalog,
        executor.clone(),
    );
    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.wait_for_requests(1).await?[0];

    let steering_content = UserInputContent::text(String::from("steer the parked tool round"));
    let steering_frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        RequestId::try_new(1)?,
        ClientRequest::SubmitInput {
            command_id: CommandId::try_from_uuid(Uuid::from_u128(0x3610))?,
            session_id: CanonicalUuid::from_uuid(fixture.session.into_uuid()),
            content: steering_content,
            expected_defaults_version: None,
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: Some(InputDelivery::Steer {
                expected_active_turn_id: CanonicalUuid::from_uuid(fixture.turn.into_uuid()),
            }),
        },
    )?;
    let steering_response = submit_frame_through_process(&fixture, &steering_frame).await?;
    let accepted_input_id: Uuid = sqlx::query_scalar(
        "SELECT accepted_input_id
           FROM accepted_input
          WHERE session_id = $1
            AND acceptance_position = 2",
    )
    .bind(fixture.session.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        steering_response,
        ServerMessage::SteeringSubmitted {
            session_id: CanonicalUuid::from_uuid(fixture.session.into_uuid()),
            accepted_input_id: CanonicalUuid::from_uuid(accepted_input_id),
            acceptance_position: CanonicalU64::new(2),
            source_turn_id: CanonicalUuid::from_uuid(fixture.turn.into_uuid()),
        }
    );

    fixture
        .decide(request, ToolApprovalDecision::Approve)
        .await?;
    execution.resume_active(fixture.session).await?;

    assert_eq!(executor.events(), vec![String::from("confirmed")]);
    assert_eq!(
        runtime.received_operations().len(),
        2,
        "the steering-bearing continuation runs exactly one more model call"
    );
    assert_eq!(
        fixture.transcript_kinds().await?,
        vec![
            "origin_accepted_input",
            "assistant_tool_use",
            "tool_execution_result",
            "steering_accepted_input",
            "assistant_text",
            "turn_completed",
        ],
        "the consumed steering entry follows the tool results in the continuation frontier"
    );
    let consumed_shape: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT accepted.disposition_kind,
                accepted.consuming_model_call_id
           FROM accepted_input AS accepted
          WHERE accepted.session_id = $1
            AND accepted.disposition_kind = 'consumed_as_steering'",
    )
    .bind(fixture.session.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(consumed_shape.0, "consumed_as_steering");
    let terminal_call: Option<Uuid> = sqlx::query_scalar(
        "SELECT terminal_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2
            AND terminal_disposition_kind = 'completed'",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        consumed_shape.1, terminal_call,
        "the completing continuation call is the steering consumer"
    );

    let mut startup = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(fixture.pool.clone()),
    );
    let recovery = startup.execute().await?;
    assert_eq!(
        recovery.recovered_turn_count(),
        0,
        "the completed steering-bearing continuation needs no recovery"
    );

    let later_turn = fixture
        .submit_new_turn(0x3611, "work after steered continuation")
        .await?;
    fixture
        .activate_and_complete_turn(later_turn, "post-steering submit completed")
        .await?;
    Ok(())
}

/// S02 / S08 / S10: steering consumed by the first model
/// call stays reconstitutable through the tool round it proposes — the parked
/// approval wait still admits submits — and a second input steers the
/// continuation, so one turn consumes steering at both safe points and the
/// completed history reloads.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s08_s10_steering_consumed_at_both_safe_points_reloads() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "confirmed",
        ToolPermissionDefault::Confirm,
        ToolEffectClass::EffectFree,
    )]);
    let executor = RecordingExecutor::completing();
    let (execution, _runtime) = fixture.execution(
        [
            tool_use_script(&[("confirmed", "{}")]),
            completion_script("doubly steered tool result observed"),
        ],
        tool_catalog,
        executor.clone(),
    );

    let sweep = PostgresEligibilitySweep::new(fixture.pool.clone());
    let (nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let mut submit = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        SubmitInputRepository::new(fixture.pool.clone()),
        nudge,
        fixture.tool_dispatch_gate.clone(),
    );
    let first_steering = submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x3710)),
            fixture.session,
            UserContent::try_text(String::from("steer the first model call"))
                .expect("fixture steering content is admitted"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: fixture.turn,
            },
        )?)
        .await?;
    assert!(
        matches!(
            first_steering,
            SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::PendingSteering(_)
            ))
        ),
        "a safe-point input against the activated turn is accepted as pending steering"
    );

    execution
        .execute(Box::new(fixture.activated.clone()))
        .await?;
    let request = fixture.wait_for_requests(1).await?[0];

    let second_steering = submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x3711)),
            fixture.session,
            UserContent::try_text(String::from("steer the continuation"))
                .expect("fixture steering content is admitted"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: fixture.turn,
            },
        )?)
        .await?;
    assert!(
        matches!(
            second_steering,
            SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::PendingSteering(_)
            ))
        ),
        "the parked round with consumed first-call steering must reconstitute for the next submit"
    );

    let mut startup = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(fixture.pool.clone()),
    );
    let recovery = startup.execute().await?;
    assert_eq!(
        recovery.recovered_turn_count(),
        0,
        "the parked approval wait with consumed steering needs no recovery"
    );

    fixture
        .decide(request, ToolApprovalDecision::Approve)
        .await?;
    execution.resume_active(fixture.session).await?;

    assert_eq!(executor.events(), vec![String::from("confirmed")]);
    assert_eq!(
        fixture.transcript_kinds().await?,
        vec![
            "origin_accepted_input",
            "steering_accepted_input",
            "assistant_tool_use",
            "tool_execution_result",
            "steering_accepted_input",
            "assistant_text",
            "turn_completed",
        ],
        "each safe point appends its consumed steering entry at its own boundary"
    );
    let consumer_count: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT consuming_model_call_id)
           FROM accepted_input
          WHERE session_id = $1
            AND disposition_kind = 'consumed_as_steering'",
    )
    .bind(fixture.session.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        consumer_count, 2,
        "the first call and the continuation call each consumed one steering input"
    );

    let later_turn = fixture
        .submit_new_turn(0x3712, "work after doubly steered turn")
        .await?;
    fixture
        .activate_and_complete_turn(later_turn, "post-double-steering submit completed")
        .await?;
    Ok(())
}

/// S06: losing a dispatched
/// external-effect attempt never retries it; startup idempotently classifies
/// exact ambiguity without projecting a result or close, and parks the turn for
/// user recovery.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s06_external_crash_parks_without_retry() -> Result<(), Box<dyn Error>> {
    let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::Disabled).await?;
    let tool_catalog = catalog([tool(
        "external_effect",
        ToolPermissionDefault::Auto,
        ToolEffectClass::ExternalEffect,
    )]);
    let crashing = SerialProbeExecutor::new();
    let (first_execution, _runtime) = fixture.execution(
        [tool_use_script(&[("external_effect", "{}")])],
        tool_catalog.clone(),
        crashing.clone(),
    );
    let activated = fixture.activated.clone();
    let abandoned_execution = tokio::spawn(async move {
        let _result = first_execution.execute(Box::new(activated)).await;
    });
    crashing.wait_for_first().await?;
    assert_eq!(crashing.events(), vec![String::from("external_effect")]);
    abandoned_execution.abort();
    let process_loss = abandoned_execution
        .await
        .expect_err("the fixture process is terminated during executor work");
    assert!(
        process_loss.is_cancelled(),
        "aborting the orchestration task models loss of same-process authority"
    );
    let abandoned: (String, Option<String>, String) = sqlx::query_as(
        "SELECT attempt.state_kind, attempt.terminal_disposition_kind,
                lifecycle.active_phase_kind
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
        abandoned,
        (String::from("in_flight"), None, String::from("running")),
        "startup receives the genuinely abandoned physical attempt"
    );

    let mut startup = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(fixture.pool.clone()),
    );
    let recovery = startup.execute().await?;
    assert_eq!(
        recovery.recovered_turn_count(),
        0,
        "the ambiguous turn remains active while its attempt is recovered"
    );
    let repeated_recovery = startup.execute().await?;
    assert_eq!(
        repeated_recovery.recovered_turn_count(),
        0,
        "repeated startup leaves the same ambiguous turn parked"
    );
    assert_eq!(crashing.events(), vec![String::from("external_effect")]);
    let post_startup_executor = RecordingExecutor::completing();
    let (post_startup_execution, post_startup_runtime) = fixture.execution(
        std::iter::empty::<Script>(),
        tool_catalog,
        post_startup_executor.clone(),
    );
    post_startup_execution
        .resume_active(fixture.session)
        .await?;
    assert!(
        post_startup_executor.events().is_empty(),
        "a progression pass must not redispatch an ambiguous external effect"
    );
    assert!(
        post_startup_runtime.received_operations().is_empty(),
        "user recovery must remain the only way beyond the ambiguous attempt"
    );
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

impl signalbox_tools_plan::SessionPlanPort for UnusedConversationPort {
    type Error = UnusedSessionStatusWriterError;

    async fn append_plan_event(
        &mut self,
        _request: signalbox_tools_plan::PlanAppendRequest,
    ) -> Result<signalbox_tools_plan::PlanAppendOutcome, Self::Error> {
        Err(UnusedSessionStatusWriterError)
    }

    async fn read_plan(
        &mut self,
        _request: signalbox_tools_plan::PlanReadRequest,
    ) -> Result<signalbox_tools_plan::PlanReadPage, Self::Error> {
        Err(UnusedSessionStatusWriterError)
    }
}
