#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt as _,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use signalbox_application::{
    EligibilityPass, InProcessAttemptDispatchGate, InProcessEligibilityWorkSource,
    InProcessToolDispatchGate, ModelCallCredentialReference, PinnedRunnerReplacementIdentities,
    PinnedRunnerReplacementOutcome, RunnerLeaseClaimRequest, RunnerLeaseResultRequest,
    StartEligibleTurnService, SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
    ToolCatalog, UuidV7StartEligibleTurnIdGenerator, UuidV7SubmitInputIdGenerator,
};
use signalbox_domain::{
    ContextFrontierId, CreateSession, DirectModelSelection, DurableCommandId, ModelCallId,
    ModelSelectionOverride, ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition,
    PerInputConfigurationChoices, ProviderModelIdentity, ReplaceLostRunner, ResolvedProviderTarget,
    RunnerCatalog, RunnerGeneration, RunnerId, RunnerLease, RunnerLeaseCorrelation,
    RunnerReplacementTarget, RunnerSandboxProfile, RunnerSelector, RunnerToolDeclaration,
    RunnerToolEffectClass, RunnerToolModelDefinition, RunnerToolPermissionOverrides,
    RunnerWorkingDirectory, SemanticTranscriptEntryId, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SessionRunnerPlacementRequest, SessionRunnerPlacementState,
    SubmitInputAppliedResult, SubmitInputResult, ToolAdmissibleLoci, ToolEffectClass, ToolName,
    ToolRequestId, TranscriptAncestry, UserContent, WorkingDirectorySelection,
    WorkspaceRequirement,
};
use signalbox_model_provider_runtime::{
    RuntimeModelCallProvider, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionEvidence, CompletionFinish, ExchangeFacts,
    ModelOperation, ModelRuntime, ObservationSink, PreparationOutcome, ProviderReportedModel,
    Script, ScriptedModel, ScriptedPrepared, TerminalEvidence, TerminalReport, TokenUsage,
    ToolCallId, ToolCallProposal, ToolName as RuntimeToolName,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    disposable_test_container_labels, local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository,
    process_read::{
        ProcessReadRepository, ProcessToolExecution, ProcessToolExecutionResultDisposition,
        ProcessTranscriptEntry,
    },
    runner_protocol::{
        PostgresExecutableToolSnapshotSource, RunnerConnectionCause, RunnerConnectionState,
        RunnerConnectionTransition, RunnerConnectionTransitionOutcome, RunnerProtocolStore,
    },
    scheduler::PostgresEligibilitySweep,
    session_credentials::{SessionCredentialPin, SessionModelCredential},
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
};
use signalbox_runner_wire::{Advertise, CanonicalUuid, Enroll, PositiveU64, Registered, Resume};
use signalbox_tools_exec::{
    CaptureCompleteness, ExecResult, ExecutionConfinement, OutputCapture, OutputEncoding,
    ProcessOutcome, SANDBOXED_EXEC_NAME, SandboxedExecTool,
};
use signalboxd::{
    ActivatedTurnExecution, ActivatedTurnPass, LocalProcessListener,
    PostgresProviderModelExecution, PostgresRunnerToolOffer, RunnerConnectionBroker,
    runner_protocol_runtime::{
        PostgresRunnerRegistrationService, RunnerEnrollmentAccepted, RunnerLeaseOperationFuture,
        RunnerLeaseOperationService, RunnerProtocolRuntime, RunnerRegistrationFuture,
        RunnerRegistrationService, RunnerResumeAccepted,
    },
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tempfile::TempDir;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt as _, runners::AsyncRunner as _},
};
use tokio::{
    io::{AsyncBufReadExt as _, BufReader},
    process::Command,
    sync::{oneshot, watch},
    time::timeout,
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_runner_process";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const PROOF_MODEL_SELECTION: u128 = 0x540;
const PROOF_MODEL_TARGET: u128 = 0x541;
const PROOF_SESSION: u128 = 0x542;
const PROOF_SESSION_COMMAND: u128 = 0x543;
const PROOF_INPUT_COMMAND: u128 = 0x544;
const PROOF_REPLACEMENT_COMMAND: u128 = 0x545;
const PROOF_REPLACEMENT_RUNNER: u128 = 0x546;
const PROOF_REPLACEMENT_ENTRY: u128 = 0x547;
const PROOF_REPLACEMENT_FRONTIER: u128 = 0x548;
const PROOF_USER_CONTENT: &str = "run the generic sandboxed exec proof";
const PROOF_OUTPUT: &str = "runner-proof";

#[derive(Clone)]
struct LossObservedRegistrationService {
    inner: PostgresRunnerRegistrationService,
    transitions: watch::Sender<Option<RunnerConnectionTransition>>,
}

impl RunnerRegistrationService for LossObservedRegistrationService {
    fn enroll(&self, request: Enroll) -> RunnerRegistrationFuture<'_, RunnerEnrollmentAccepted> {
        self.inner.enroll(request)
    }

    fn resume(&self, request: Resume) -> RunnerRegistrationFuture<'_, RunnerResumeAccepted> {
        self.inner.resume(request)
    }

    fn advertise(
        &self,
        enrollment: CanonicalUuid,
        request: Advertise,
        epoch: PositiveU64,
    ) -> RunnerRegistrationFuture<'_, Registered> {
        self.inner.advertise(enrollment, request, epoch)
    }

    fn transition_connection(
        &self,
        enrollment: CanonicalUuid,
        epoch: PositiveU64,
        transition: RunnerConnectionTransition,
    ) -> RunnerRegistrationFuture<'_, RunnerConnectionTransitionOutcome> {
        let inner = self.inner.clone();
        let transitions = self.transitions.clone();
        Box::pin(async move {
            let outcome = inner
                .transition_connection(enrollment, epoch, transition)
                .await?;
            let _ = transitions.send(Some(transition));
            Ok(outcome)
        })
    }
}

#[derive(Clone)]
struct ResultObservedLeaseOperations {
    inner: RunnerProtocolStore,
    completed: Arc<Mutex<Option<oneshot::Sender<RunnerLeaseCorrelation>>>>,
}

impl RunnerLeaseOperationService for ResultObservedLeaseOperations {
    fn claim(
        &self,
        request: RunnerLeaseClaimRequest,
    ) -> RunnerLeaseOperationFuture<'_, RunnerLease> {
        RunnerLeaseOperationService::claim(&self.inner, request)
    }

    fn record_result(
        &self,
        request: RunnerLeaseResultRequest,
    ) -> RunnerLeaseOperationFuture<'_, RunnerLease> {
        let inner = self.inner.clone();
        let completed = Arc::clone(&self.completed);
        Box::pin(async move {
            let lease = RunnerLeaseOperationService::record_result(&inner, request).await?;
            let correlation = lease.correlation();
            completed
                .lock()
                .expect("the result-observation sender lock remains available")
                .take()
                .expect("the runtime records one terminal runner result")
                .send(correlation)
                .expect("the result observer remains live");
            Ok(lease)
        })
    }
}

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

fn runner_configuration(
    socket: &std::path::Path,
    runner_root: &std::path::Path,
    supervisor: &std::path::Path,
    bubblewrap: &std::path::Path,
    proof_configuration: &str,
) -> String {
    format!(
        r#"version = 1
{proof_configuration}daemon_socket_path = "{}"
runner_root = "{}"
exec_supervisor_executable = "{}"
bubblewrap_path = "{}"
read_only_paths = ["/usr"]
allowed_network_hosts = []
git_author_name = "Signalbox Test Runner"
git_author_email = "runner-test@example.invalid"
credentials = {{}}
repositories = {{}}
"#,
        socket.display(),
        runner_root.display(),
        supervisor.display(),
        bubblewrap.display(),
    )
}

fn tool_use_script(arguments: &str) -> Script {
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new("scripted-runner-proof")),
        finish: CompletionFinish::ToolUse,
        content: vec![AssistantPart::ToolCall(ToolCallProposal {
            id: ToolCallId::new("runner-proof-call"),
            name: RuntimeToolName::new(SANDBOXED_EXEC_NAME),
            arguments_json: arguments.to_owned(),
        })],
        usage: TokenUsage::unreported(),
    }))
}

fn completion_script() -> Script {
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new("scripted-runner-proof")),
        finish: CompletionFinish::EndTurn,
        content: vec![AssistantPart::Text(String::from("runner result observed"))],
        usage: TokenUsage::unreported(),
    }))
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct RunnerLossFacts {
    state_kind: String,
    pinned_runner_id: uuid::Uuid,
    loss_source_kind: String,
    placement_revision: i64,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct ReplacementCounts {
    staged_rows: i64,
    terminal_results: i64,
}

async fn kill_runner_and_wait_for_loss(
    runner: &mut tokio::process::Child,
    loss_observer: &mut watch::Receiver<Option<RunnerConnectionTransition>>,
) -> Result<(), Box<dyn Error>> {
    runner.start_kill()?;
    timeout(PROCESS_TIMEOUT, runner.wait())
        .await
        .expect("the proof runner stops before the loss deadline")?;
    timeout(
        PROCESS_TIMEOUT,
        loss_observer
            .wait_for(|observed| observed == &Some(RunnerConnectionTransition::TransportClosed)),
    )
    .await
    .expect("the daemon propagates runner loss before the integration deadline")
    .expect("the loss observer remains connected");
    Ok(())
}

async fn postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_fsync_enabled()
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

fn private_tempdir() -> Result<TempDir, Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
    Ok(directory)
}

/// S30 / INV-042: the packaged runner process enrolls through the daemon's
/// local wire and leaves its committed registration reconstitutable.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and the packaged signalbox-runner binary"]
async fn s30_inv042_spawned_runner_enrolls_against_durable_daemon() -> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
    let directory = private_tempdir()?;
    let socket = directory.path().join("runner.sock");
    let runner_root = directory.path().join("runner-state");
    let configuration_path = directory.path().join("runner.toml");
    let runner_binary = env!("CARGO_BIN_EXE_signalbox-runner");
    let configuration = format!(
        r#"version = 1
daemon_socket_path = "{}"
runner_root = "{}"
exec_supervisor_executable = "{runner_binary}"
bubblewrap_path = "{runner_binary}"
read_only_paths = ["/usr"]
allowed_network_hosts = []
git_author_name = "Signalbox Test Runner"
git_author_email = "runner-test@example.invalid"
credentials = {{}}
repositories = {{}}
"#,
        socket.display(),
        runner_root.display(),
    );
    fs::write(&configuration_path, configuration)?;

    let listener = LocalProcessListener::bind(&socket)?;
    let service = PostgresRunnerRegistrationService::registration_only(pool.clone())
        .expect("the registration-only runner catalog is valid");
    let store = service.protocol_store();
    let (shutdown_sender, shutdown) = watch::channel(false);
    let runtime = tokio::spawn(RunnerProtocolRuntime::new(listener, service).run(shutdown));
    let mut runner = Command::new(runner_binary)
        .arg("--config")
        .arg(&configuration_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stderr = runner.stderr.take().expect("the runner stderr is piped");
    let mut stderr = BufReader::new(stderr);
    let mut enrollment_line = String::new();
    timeout(PROCESS_TIMEOUT, stderr.read_line(&mut enrollment_line)).await??;

    assert!(
        enrollment_line.contains("runner enrolled"),
        "unexpected runner output: {enrollment_line}"
    );
    let connections = store.load_nonterminal_connection_heads().await?;
    assert_eq!(connections.len(), 1);
    let enrollment = store.load_enrollment(connections[0].enrollment()).await?;
    assert!(enrollment.is_some());

    shutdown_sender.send(true)?;
    timeout(PROCESS_TIMEOUT, runtime)
        .await
        .expect("the daemon runner runtime stops before the integration deadline")
        .expect("the daemon runner runtime task joins")?;
    let runner_status = timeout(PROCESS_TIMEOUT, runner.wait())
        .await
        .expect("the runner process stops after the daemon shutdown")?;
    assert!(runner_status.success());
    pool.close().await;
    drop(container);
    Ok(())
}

/// S32 / INV-042 / INV-044: physical loss of the spawned runner durably marks
/// its exact unpinned session placement lost before the daemon can shut down.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and the packaged signalbox-runner binary"]
async fn s32_inv042_inv044_spawned_runner_loss_reaches_its_placed_session()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
    let directory = private_tempdir()?;
    let socket = directory.path().join("runner.sock");
    let runner_root = directory.path().join("runner-state");
    let working_directory = directory.path().join("session-workspace");
    let configuration_path = directory.path().join("runner.toml");
    let runner_binary = env!("CARGO_BIN_EXE_signalbox-runner");
    let configuration = format!(
        r#"version = 1
daemon_socket_path = "{}"
runner_root = "{}"
exec_supervisor_executable = "{runner_binary}"
bubblewrap_path = "{runner_binary}"
read_only_paths = ["/usr"]
allowed_network_hosts = []
git_author_name = "Signalbox Test Runner"
git_author_email = "runner-test@example.invalid"
credentials = {{}}
repositories = {{}}
"#,
        socket.display(),
        runner_root.display(),
    );
    fs::write(&configuration_path, configuration)?;

    let listener = LocalProcessListener::bind(&socket)?;
    let inner = PostgresRunnerRegistrationService::registration_only(pool.clone())
        .expect("the registration-only runner catalog is valid");
    let store = inner.protocol_store();
    let (loss_sender, mut loss_observer) = watch::channel(None);
    let service = LossObservedRegistrationService {
        inner,
        transitions: loss_sender,
    };
    let (shutdown_sender, shutdown) = watch::channel(false);
    let runtime = tokio::spawn(RunnerProtocolRuntime::new(listener, service).run(shutdown));
    let mut runner = Command::new(runner_binary)
        .arg("--config")
        .arg(&configuration_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stderr = runner.stderr.take().expect("the runner stderr is piped");
    let mut stderr = BufReader::new(stderr);
    let mut enrollment_line = String::new();
    timeout(PROCESS_TIMEOUT, stderr.read_line(&mut enrollment_line)).await??;
    let connections = store.load_nonterminal_connection_heads().await?;
    assert_eq!(connections.len(), 1);
    let enrollment = store
        .load_enrollment(connections[0].enrollment())
        .await?
        .expect("the runner enrollment is durable after its acknowledgement");
    let session = SessionId::from_uuid(uuid::Uuid::from_u128(0x530));
    let placement = SessionRunnerPlacementRequest {
        selector: RunnerSelector::Identity(enrollment.runner()),
        working_directory: WorkingDirectorySelection::Exact(
            RunnerWorkingDirectory::try_new(working_directory.display().to_string())
                .expect("the absolute fixture working directory is valid"),
        ),
        credential_profile: None,
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: RunnerToolPermissionOverrides::try_new([])
            .expect("the empty permission override inventory is valid"),
    };
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(uuid::Uuid::from_u128(0x531)),
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(uuid::Uuid::from_u128(0x532)),
        )),
    )
    .with_runner_placement(Some(placement))
    .prepare(session)
    .expect("the exact runner-placed session is preparable");
    let credentials = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "fixture-model-family",
        "fixture-credential-reference",
    )])
    .expect("the synthetic model credential pin is valid");
    CreateSessionRepository::new(pool.clone(), credentials)
        .handle(creation)
        .await?;

    runner.start_kill()?;
    timeout(PROCESS_TIMEOUT, runner.wait())
        .await
        .expect("the runner process stops before the loss deadline")?;
    timeout(
        PROCESS_TIMEOUT,
        loss_observer
            .wait_for(|observed| observed == &Some(RunnerConnectionTransition::TransportClosed)),
    )
    .await
    .expect("the daemon propagates runner loss before the integration deadline")
    .expect("the loss observer remains connected");
    let connection = store
        .load_connection(enrollment.enrollment())
        .await?
        .expect("the terminal connection lifecycle is durable");
    let lost_placement = store
        .load_placement(session)
        .await?
        .expect("the runner placement remains reconstitutable after loss");

    assert!(enrollment_line.contains("runner enrolled"));
    assert_eq!(connection.state(), RunnerConnectionState::Lost);
    assert_eq!(connection.cause(), RunnerConnectionCause::TransportClosed);
    assert_eq!(
        lost_placement.placement().state(),
        &SessionRunnerPlacementState::RunnerLostBeforePin(
            signalbox_domain::RunnerLostBeforePin::from_stored(enrollment.runner())
        )
    );

    shutdown_sender.send(true)?;
    timeout(PROCESS_TIMEOUT, runtime)
        .await
        .expect("the daemon runner runtime stops before the integration deadline")
        .expect("the daemon runner runtime task joins")?;
    pool.close().await;
    drop(container);
    Ok(())
}

/// S30 / S32 / INV-042 / INV-043 / INV-044: a session placed on an actually
/// spawned runner completes one generic exec-family call inside the restricted
/// bubblewrap profile, retains the exact runner execution object, then crosses
/// durable loss and stages one replay-safe replacement command.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL, bubblewrap, and the packaged runner binaries"]
async fn s30_s32_inv042_inv043_inv044_spawned_runner_executes_then_stages_replacement()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
    let directory = private_tempdir()?;
    let socket = directory.path().join("runner.sock");
    let runner_root = directory.path().join("runner-state");
    let working_directory = directory.path().join("session-workspace");
    let configuration_path = directory.path().join("runner.toml");
    let runner_binary = std::path::Path::new(env!("CARGO_BIN_EXE_signalbox-runner"));
    let supervisor_binary = std::path::Path::new(env!("CARGO_BIN_EXE_signalbox-exec-supervisor"));
    let bubblewrap_binary = std::path::Path::new("/usr/bin/bwrap");
    fs::create_dir(&working_directory)?;
    fs::write(
        &configuration_path,
        runner_configuration(
            &socket,
            &runner_root,
            supervisor_binary,
            bubblewrap_binary,
            "",
        ),
    )?;

    let bootstrap_listener = LocalProcessListener::bind(&socket)?;
    let bootstrap_service = PostgresRunnerRegistrationService::registration_only(pool.clone())
        .expect("the registration-only runner catalog is valid");
    let bootstrap_store = bootstrap_service.protocol_store();
    let (bootstrap_shutdown_sender, bootstrap_shutdown) = watch::channel(false);
    let bootstrap_runtime = tokio::spawn(
        RunnerProtocolRuntime::new(bootstrap_listener, bootstrap_service).run(bootstrap_shutdown),
    );
    let mut bootstrap_runner = Command::new(runner_binary)
        .arg("--config")
        .arg(&configuration_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let bootstrap_stderr = bootstrap_runner
        .stderr
        .take()
        .expect("the bootstrap runner stderr is piped");
    let mut bootstrap_stderr = BufReader::new(bootstrap_stderr);
    let mut enrollment_line = String::new();
    timeout(
        PROCESS_TIMEOUT,
        bootstrap_stderr.read_line(&mut enrollment_line),
    )
    .await??;
    let bootstrap_connections = bootstrap_store.load_nonterminal_connection_heads().await?;
    assert_eq!(bootstrap_connections.len(), 1);
    let enrollment = bootstrap_store
        .load_enrollment(bootstrap_connections[0].enrollment())
        .await?
        .expect("the proof runner enrollment is durable");

    bootstrap_shutdown_sender.send(true)?;
    timeout(PROCESS_TIMEOUT, bootstrap_runtime)
        .await
        .expect("the bootstrap daemon runtime stops before the integration deadline")
        .expect("the bootstrap daemon runtime task joins")?;
    let bootstrap_status = timeout(PROCESS_TIMEOUT, bootstrap_runner.wait())
        .await
        .expect("the bootstrap runner stops after daemon shutdown")?;
    assert!(bootstrap_status.success());

    let sandboxed = SandboxedExecTool::try_new_production_with_bubblewrap(
        &working_directory,
        supervisor_binary,
        bubblewrap_binary,
    )?;
    let (tool_catalog, tool_executor) = sandboxed.into_parts();
    let tool_name = ToolName::try_new(SANDBOXED_EXEC_NAME.to_owned())
        .expect("the committed generic exec-family tool name is valid");
    let tool_definition = tool_catalog
        .definition(&tool_name)
        .expect("the generic exec-family catalog contains its declaration");
    assert_eq!(
        tool_definition.effect_class(),
        ToolEffectClass::ExternalEffect
    );
    let runner_catalog = RunnerCatalog::try_new(
        [],
        [RunnerToolDeclaration::new(
            tool_name.clone(),
            RunnerToolModelDefinition::try_new(
                tool_definition.description().to_owned(),
                tool_definition.input_schema().as_str().to_owned(),
            )
            .expect("the compiled exec-family model definition is runner-admissible"),
            tool_definition.permission_default(),
            RunnerToolEffectClass::SideEffecting,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::Identity(enrollment.runner()),
            },
        )],
        [],
        [],
        [RunnerSandboxProfile::WorkspaceRestricted],
    )
    .expect("the exact proof runner catalog is internally consistent");
    let store = RunnerProtocolStore::new(pool.clone(), runner_catalog);
    let (loss_sender, mut loss_observer) = watch::channel(None);
    let service = LossObservedRegistrationService {
        inner: PostgresRunnerRegistrationService::new(store.clone(), []),
        transitions: loss_sender,
    };
    let broker = RunnerConnectionBroker::new();
    let (result_sender, result_observer) = oneshot::channel();
    let operations = ResultObservedLeaseOperations {
        inner: store.clone(),
        completed: Arc::new(Mutex::new(Some(result_sender))),
    };
    let listener = LocalProcessListener::bind(&socket)?;
    let (shutdown_sender, shutdown) = watch::channel(false);
    let runtime = tokio::spawn(
        RunnerProtocolRuntime::new(listener, service)
            .with_lease_operation_service(operations)
            .with_connection_broker(broker.clone())
            .run(shutdown),
    );
    fs::write(
        &configuration_path,
        runner_configuration(
            &socket,
            &runner_root,
            supervisor_binary,
            bubblewrap_binary,
            "execution_proof = \"generic_sandboxed_exec\"\n",
        ),
    )?;
    let mut runner = Command::new(runner_binary)
        .arg("--config")
        .arg(&configuration_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stderr = runner
        .stderr
        .take()
        .expect("the proof runner stderr is piped");
    let mut stderr = BufReader::new(stderr);
    let mut resume_line = String::new();
    timeout(PROCESS_TIMEOUT, stderr.read_line(&mut resume_line)).await??;
    assert!(resume_line.contains("runner resumed"));

    let session = SessionId::from_uuid(uuid::Uuid::from_u128(PROOF_SESSION));
    let selection = DirectModelSelection::from_uuid(uuid::Uuid::from_u128(PROOF_MODEL_SELECTION));
    let placement = SessionRunnerPlacementRequest {
        selector: RunnerSelector::Identity(enrollment.runner()),
        working_directory: WorkingDirectorySelection::Exact(
            RunnerWorkingDirectory::try_new(working_directory.display().to_string())
                .expect("the absolute proof working directory is valid"),
        ),
        credential_profile: None,
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::WorkspaceRestricted,
        permission_overrides: RunnerToolPermissionOverrides::try_new([])
            .expect("the proof carries no runner permission override"),
    };
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(uuid::Uuid::from_u128(PROOF_SESSION_COMMAND)),
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
    )
    .with_runner_placement(Some(placement))
    .prepare(session)
    .expect("the proof runner-placed session is preparable");
    let credentials = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "proof-model-family",
        "proof-model-credential-reference",
    )])
    .expect("the synthetic proof model credential pin is valid");
    CreateSessionRepository::new(pool.clone(), credentials)
        .handle(creation)
        .await?;

    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let tool_dispatch_gate = InProcessToolDispatchGate::default();
    let mut submit = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        SubmitInputRepository::new(pool.clone()),
        nudge,
        tool_dispatch_gate.clone(),
    );
    let submitted = submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(PROOF_INPUT_COMMAND)),
            session,
            UserContent::try_text(PROOF_USER_CONTENT.to_owned())
                .expect("the proof user content is admitted"),
            signalbox_domain::DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )?)
        .await?;
    assert!(matches!(
        submitted,
        SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    let start = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );

    let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        uuid::Uuid::from_u128(PROOF_MODEL_TARGET),
    ));
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("the proof model target is unique");
    let runtime_models =
        RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
            target,
            String::from("scripted-runner-proof"),
            64,
            200_000,
        )?])?;
    let arguments = serde_json::json!({
        "program": "/usr/bin/printf",
        "arguments": [PROOF_OUTPUT],
        "timeout_seconds": 5,
    })
    .to_string();
    let model_runtime = Arc::new(ScriptedModel::<ModelCallId>::following([
        tool_use_script(&arguments),
        completion_script(),
    ]));
    let provider = RuntimeModelCallProvider::new(
        RecordingScriptedModel {
            inner: Arc::clone(&model_runtime),
        },
        runtime_models,
    );
    let execution = PostgresProviderModelExecution::new(
        PostgresModelCallRepository::new(
            pool.clone(),
            targets,
            ModelCallCredentialReference::new("proof-model-credential-reference"),
        ),
        InProcessAttemptDispatchGate::default(),
        provider,
    )
    .with_tool_loop(tool_dispatch_gate, tool_catalog, tool_executor)
    .with_runner_tool_offer(PostgresRunnerToolOffer::new(store.clone(), broker))
    .with_executable_tool_snapshot_source(PostgresExecutableToolSnapshotSource::new(store.clone()));
    ActivatedTurnPass::new(start, execution.clone())
        .run(session)
        .await?;
    let completed_correlation = timeout(PROCESS_TIMEOUT, result_observer)
        .await
        .expect("the runner result commits before the integration deadline")?;
    execution.resume_active(session).await?;

    let transcript = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the proof session transcript is readable");
    assert_eq!(transcript.entries().len(), 5);
    let (result_entry, result_request): (uuid::Uuid, uuid::Uuid) = sqlx::query_as(
        "SELECT entry.semantic_entry_id, attempt.request_id
           FROM semantic_transcript_entry AS entry
           JOIN tool_attempt AS attempt
             ON attempt.attempt_id = entry.tool_result_attempt_id
          WHERE entry.source_session_id = $1
            AND entry.payload_kind = 'tool_execution_result'",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let expected_result = serde_json::to_string(&ExecResult {
        confinement: ExecutionConfinement::FilesystemConfined,
        outcome: ProcessOutcome::Exited { code: Some(0) },
        stdout: OutputCapture {
            text: PROOF_OUTPUT.to_owned(),
            completeness: CaptureCompleteness::Complete,
            encoding: OutputEncoding::Utf8,
        },
        stderr: OutputCapture {
            text: String::new(),
            completeness: CaptureCompleteness::Complete,
            encoding: OutputEncoding::Utf8,
        },
    })?;
    assert_eq!(
        transcript.entries()[2],
        ProcessTranscriptEntry::ToolExecutionResult {
            entry_index: 2,
            source_session: session,
            entry: signalbox_domain::SemanticTranscriptEntryId::from_uuid(result_entry),
            request: ToolRequestId::from_uuid(result_request),
            attempt: completed_correlation.dispatch.attempt(),
            disposition: ProcessToolExecutionResultDisposition::Completed,
            execution: ProcessToolExecution::Runner {
                runner: enrollment.runner(),
                lease: completed_correlation.lease,
                placement_revision: completed_correlation.placement_revision,
                lease_generation: completed_correlation.generation,
                working_directory: Some(completed_correlation.working_directory),
                sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            },
            content: expected_result,
        }
    );
    assert_eq!(model_runtime.received_operations().len(), 2);

    kill_runner_and_wait_for_loss(&mut runner, &mut loss_observer).await?;
    let lost_placement = store
        .load_placement(session)
        .await?
        .expect("the pinned proof placement remains reconstitutable after loss");
    let loss_facts: RunnerLossFacts = sqlx::query_as(
        "SELECT placement.state_kind AS state_kind,
                placement.pinned_runner_id AS pinned_runner_id,
                placement.loss_source_kind AS loss_source_kind,
                placement.placement_revision::bigint AS placement_revision
           FROM runner_current_session_placement AS current_head
           JOIN runner_session_placement_record AS placement
             ON placement.session_id = current_head.session_id
            AND placement.event_ordinal = current_head.event_ordinal
          WHERE current_head.session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        lost_placement.placement().revision(),
        completed_correlation.placement_revision
    );
    assert_eq!(loss_facts.state_kind, "runner_lost");
    assert_eq!(loss_facts.pinned_runner_id, enrollment.runner().into_uuid());
    assert_eq!(loss_facts.loss_source_kind, "connection");
    assert_eq!(
        loss_facts.placement_revision,
        i64::try_from(completed_correlation.placement_revision.get())
            .expect("the proof placement revision fits PostgreSQL bigint")
    );

    let replacement_command = ReplaceLostRunner::new(
        DurableCommandId::from_uuid(uuid::Uuid::from_u128(PROOF_REPLACEMENT_COMMAND)),
        session,
        completed_correlation.placement_revision,
        RunnerReplacementTarget::Runner(RunnerId::from_uuid(uuid::Uuid::from_u128(
            PROOF_REPLACEMENT_RUNNER,
        ))),
    );
    let replacement_identities = PinnedRunnerReplacementIdentities::new(
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::from_u128(PROOF_REPLACEMENT_ENTRY)),
        ContextFrontierId::from_uuid(uuid::Uuid::from_u128(PROOF_REPLACEMENT_FRONTIER)),
    );
    let staged = store
        .stage_workspace_free_pinned_replacement(replacement_command, replacement_identities)
        .await?;
    let replayed = store
        .stage_workspace_free_pinned_replacement(replacement_command, replacement_identities)
        .await?;
    let replacement_counts: ReplacementCounts = sqlx::query_as(
        "SELECT
             (SELECT count(*) FROM runner_workspace_free_replacement_stage
               WHERE command_id = $1) AS staged_rows,
             (SELECT count(*) FROM replace_lost_runner_result
               WHERE command_id = $1) AS terminal_results",
    )
    .bind(replacement_command.command().into_uuid())
    .fetch_one(&pool)
    .await?;
    let expected_stage = PinnedRunnerReplacementOutcome::Staged {
        command: replacement_command.command(),
    };
    assert_eq!(staged, expected_stage);
    assert_eq!(replayed, expected_stage);
    assert_eq!(
        replacement_counts,
        ReplacementCounts {
            staged_rows: 1,
            terminal_results: 0,
        }
    );

    shutdown_sender.send(true)?;
    timeout(PROCESS_TIMEOUT, runtime)
        .await
        .expect("the proof daemon runtime stops before the integration deadline")
        .expect("the proof daemon runtime task joins")?;
    pool.close().await;
    drop(container);
    Ok(())
}
