#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

mod support;

use std::{
    error::Error,
    fmt, fs,
    io::{self, ErrorKind},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use rustix::process::{Pid, Signal, kill_process};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    InProcessAttemptDispatchGate, InProcessEligibilityWorkSource, InProcessToolDispatchGate,
    ModelCallCredentialReference, OperatorFailureClass, SchedulerLoop, SchedulerLoopExit,
    StartEligibleTurnOutcome, StartEligibleTurnService, ToolDefinition, ToolExecutionInvocation,
    ToolExecutor, ToolExecutorEvidence, ToolInputSchema, UuidV7StartEligibleTurnIdGenerator,
};
use signalbox_domain::{
    DirectModelSelection, ModelTargetCatalog, ModelTargetDefinition, NormalizedToolArguments,
    ProviderModelIdentity, ResolvedProviderTarget, SessionId, ToolEffectClass,
    ToolExecutionErrorDetail, ToolName, ToolPermissionDefault,
};
use signalbox_model_provider_runtime::{
    RuntimeModelCallProvider, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, ExchangeFacts, ObservationFact,
    ProviderReportedModel, Script, ScriptedModel, TerminalEvidence, TokenUsage, ToolCallId,
    ToolCallProposal as RuntimeToolCallProposal, ToolName as RuntimeToolName,
};
use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository, scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository,
};
use signalbox_test_bin::test_bin_path;
use signalboxd::{
    ActivatedTurnPass, FatalExecutionSupervisor, LocalProcessListener,
    PostgresProviderModelExecution, ProcessRuntime, ProcessRuntimeError,
    WorkspaceInstructionRuntime,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    process::Command,
    sync::watch,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_terminal_chat";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const SCRIPTED_PROVIDER: &str = "scripted-chat";
const TOOL_NAME: &str = "confirmed_probe";
const APPROVAL_INPUT_LINE: &str = "use the confirmed probe\n";
const INITIAL_INPUT_LINE: &str = "first user line\n";
const STEERING_INPUT_LINE: &str = ":steer inspect the cache\n";
const STOP_INPUT_LINE: &str = ":stop successor user line\n";
const FIRST_DELTA: &str = "checking ";
const FINAL_REPLY: &str = "approved tool reply";
const COMPACTION_PROMPT: &str = "Summarize the prior conversation faithfully for continuation.";
const CONTEXT_WINDOW_TOKENS: u32 = 200_000;

struct SocketDirectory {
    directory: PathBuf,
    socket: PathBuf,
}

impl SocketDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let directory = PathBuf::from("/tmp").join(format!("signalbox-chat-{}", Uuid::now_v7()));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let socket = directory.join("signalbox.sock");
        Ok(Self { directory, socket })
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn cleanup(self) -> Result<(), Box<dyn Error>> {
        let mut lock = self.socket.into_os_string();
        lock.push(".lock");
        match fs::remove_file(PathBuf::from(lock)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::remove_dir(self.directory)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct CompletingFixtureExecutor;

#[derive(Debug)]
struct FixtureExecutorError;

impl fmt::Display for FixtureExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the completing fixture executor cannot fail")
    }
}

impl Error for FixtureExecutorError {}

impl ClassifyOperatorFailure for FixtureExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        }
    }
}

impl ToolExecutor for CompletingFixtureExecutor {
    type Error = FixtureExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let name = invocation.request().name().as_str().to_owned();
        Ok(invocation.bind(ToolExecutorEvidence::CompletedText(format!(
            "completed:{name}"
        ))))
    }
}

struct RunningChatFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    socket_directory: SocketDirectory,
    selection: DirectModelSelection,
    shutdown: watch::Sender<bool>,
    process_task: JoinHandle<Result<(), ProcessRuntimeError>>,
    scheduler_task: JoinHandle<SchedulerLoopExit>,
}

struct RunningIdleFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    socket_directory: SocketDirectory,
    selection: DirectModelSelection,
    shutdown: watch::Sender<bool>,
    process_task: JoinHandle<Result<(), ProcessRuntimeError>>,
    _work_source: InProcessEligibilityWorkSource<PostgresEligibilitySweep>,
}

impl RunningIdleFixture {
    async fn start() -> Result<Self, Box<dyn Error>> {
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
            .max_connections(12)
            .connect_with(local_test_connection_options(&database_url)?)
            .await?;
        migrate(&pool).await?;

        let socket_directory = SocketDirectory::create()?;
        let selection_uuid = Uuid::from_u128(0x9501);
        let target_uuid = Uuid::from_u128(0x9502);
        let selection = DirectModelSelection::from_uuid(selection_uuid);
        let model_configuration = support::parse_model_configuration(&format!(
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
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "{COMPACTION_PROMPT}"

[[models]]
selection_id = "{selection_uuid}"
target_id = "{target_uuid}"
model_family = "anthropic"
provider_model = "idle-chat"
max_output_tokens = 64
context_window_tokens = {CONTEXT_WINDOW_TOKENS}
"#,
        ))?;
        let sweep = PostgresEligibilitySweep::new(pool.clone());
        let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
        let listener = LocalProcessListener::bind(socket_directory.socket())?;
        let process_runtime = ProcessRuntime::new(
            listener,
            pool.clone(),
            eligibility_nudge,
            InProcessToolDispatchGate::default(),
            model_configuration,
        );
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let process_task = tokio::spawn(process_runtime.run(shutdown_receiver));
        Ok(Self {
            container,
            pool,
            socket_directory,
            selection,
            shutdown,
            process_task,
            _work_source: work_source,
        })
    }

    async fn create_session(&self) -> Result<String, Box<dyn Error>> {
        create_session(self.socket_directory.socket(), self.selection.into_uuid()).await
    }

    async fn stop(self) -> Result<(), Box<dyn Error>> {
        self.shutdown.send(true)?;
        timeout(Duration::from_secs(10), self.process_task).await???;
        self.pool.close().await;
        self.socket_directory.cleanup()?;
        drop(self.container);
        Ok(())
    }
}

impl RunningChatFixture {
    async fn start() -> Result<Self, Box<dyn Error>> {
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
            .max_connections(12)
            .connect_with(local_test_connection_options(&database_url)?)
            .await?;
        migrate(&pool).await?;

        let socket_directory = SocketDirectory::create()?;
        let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x9401));
        let target_uuid = Uuid::from_u128(0x9402);
        let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(target_uuid));
        let model_configuration = support::parse_model_configuration(&format!(
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
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "{COMPACTION_PROMPT}"

[[models]]
selection_id = "{}"
target_id = "{}"
model_family = "anthropic"
provider_model = "{SCRIPTED_PROVIDER}"
max_output_tokens = 64
context_window_tokens = {CONTEXT_WINDOW_TOKENS}
"#,
            selection.into_uuid(),
            target_uuid,
        ))?;
        let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
            selection, target,
        )])
        .expect("the fixture target definition is unique");
        let runtime_models =
            RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
                target,
                String::from(SCRIPTED_PROVIDER),
                64,
                CONTEXT_WINDOW_TOKENS,
            )
            .expect("the fixture runtime definition is valid")])
            .expect("the fixture runtime target is unique");
        let first = Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(SCRIPTED_PROVIDER)),
            finish: CompletionFinish::ToolUse,
            content: vec![
                AssistantPart::Text(String::from(FIRST_DELTA)),
                AssistantPart::ToolCall(RuntimeToolCallProposal {
                    id: ToolCallId::new(String::from("fixture-call-0")),
                    name: RuntimeToolName::new(TOOL_NAME),
                    arguments_json: String::from("{}"),
                }),
            ],
            usage: TokenUsage::unreported(),
        }))
        .observing(ObservationFact::TextDelta {
            index: 0,
            text: String::from(FIRST_DELTA),
        });
        let second = Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new(SCRIPTED_PROVIDER)),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(String::from(FINAL_REPLY))],
            usage: TokenUsage::unreported(),
        }))
        .observing(ObservationFact::TextDelta {
            index: 0,
            text: String::from(FINAL_REPLY),
        });
        let scripted = ScriptedModel::following([first, second]);
        let tool_catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            ToolDefinition::new(
                ToolName::try_new(String::from(TOOL_NAME)).expect("the fixture tool name is valid"),
                String::from("Runs the confirm-classified fixture probe."),
                ToolInputSchema::try_new(String::from(
                    r#"{"additionalProperties":true,"type":"object"}"#,
                ))
                .expect("the fixture schema is valid"),
                ToolPermissionDefault::Confirm,
                ToolEffectClass::EffectFree,
            ),
            |_arguments: &NormalizedToolArguments| Ok::<(), ToolExecutionErrorDetail>(()),
        )])
        .expect("the fixture tool declaration is unique");

        let sweep = PostgresEligibilitySweep::new(pool.clone());
        let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
        let tool_dispatch_gate = InProcessToolDispatchGate::default();
        let listener = LocalProcessListener::bind(socket_directory.socket())?;
        let process_runtime = ProcessRuntime::new(
            listener,
            pool.clone(),
            eligibility_nudge,
            tool_dispatch_gate.clone(),
            model_configuration,
        );
        let provider = RuntimeModelCallProvider::new(scripted, runtime_models, None)
            .with_text_delta_sink(process_runtime.provider_text_delta_sink());
        let (execution, _) = FatalExecutionSupervisor::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    pool.clone(),
                    targets,
                    ModelCallCredentialReference::new(SCRIPTED_PROVIDER),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            )
            .with_tool_loop(tool_dispatch_gate, tool_catalog, CompletingFixtureExecutor)
            .with_workspace_instructions(WorkspaceInstructionRuntime::new(
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
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let process_task = tokio::spawn(process_runtime.run(shutdown_receiver.clone()));
        let scheduler_task = tokio::spawn(async move {
            scheduler
                .run_until(wait_for_shutdown(shutdown_receiver))
                .await
        });
        Ok(Self {
            container,
            pool,
            socket_directory,
            selection,
            shutdown,
            process_task,
            scheduler_task,
        })
    }

    async fn create_session(&self) -> Result<String, Box<dyn Error>> {
        let output = Command::new(test_bin_path!("signalbox"))
            .env_remove("SIGNALBOX_SOCKET_PATH")
            .arg("--socket")
            .arg(self.socket_directory.socket())
            .arg("create")
            .arg("--model")
            .arg(self.selection.into_uuid().hyphenated().to_string())
            .output()
            .await?;
        if !output.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&output.stderr)).into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    async fn stop(self) -> Result<(), Box<dyn Error>> {
        self.shutdown.send(true)?;
        assert_eq!(
            timeout(Duration::from_secs(10), self.scheduler_task).await??,
            SchedulerLoopExit::Shutdown
        );
        timeout(Duration::from_secs(10), self.process_task).await???;
        self.pool.close().await;
        self.socket_directory.cleanup()?;
        drop(self.container);
        Ok(())
    }
}

async fn activate_turn(pool: &PgPool, session_id: Uuid) -> Result<(), Box<dyn Error>> {
    let mut activation = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    match activation.execute(SessionId::from_uuid(session_id)).await? {
        StartEligibleTurnOutcome::Activated(_) => Ok(()),
        StartEligibleTurnOutcome::NoEligibleTurn => {
            Err(io::Error::other("the chat input did not leave an eligible turn").into())
        }
    }
}

async fn create_session(socket: &Path, selection: Uuid) -> Result<String, Box<dyn Error>> {
    let output = Command::new(test_bin_path!("signalbox"))
        .env_remove("SIGNALBOX_SOCKET_PATH")
        .arg("--socket")
        .arg(socket)
        .arg("create")
        .arg("--model")
        .arg(selection.hyphenated().to_string())
        .output()
        .await?;
    if !output.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn read_through<R>(
    lines: &mut Lines<R>,
    rendered: &mut Vec<String>,
    needle: &str,
) -> Result<String, Box<dyn Error>>
where
    R: AsyncBufRead + Unpin,
{
    timeout(Duration::from_secs(20), async {
        loop {
            let line = lines
                .next_line()
                .await?
                .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "chat stdout closed"))?;
            rendered.push(line.clone());
            if line.contains(needle) {
                return Ok::<String, io::Error>(line);
            }
        }
    })
    .await
    .map_err(|_| {
        io::Error::other(format!(
            "chat did not print {needle}; rendered={rendered:?}"
        ))
    })?
    .map_err(Into::into)
}

fn line_position(lines: &[String], needle: &str) -> Result<usize, Box<dyn Error>> {
    lines
        .iter()
        .position(|line| line.contains(needle))
        .ok_or_else(|| io::Error::other(format!("output omitted {needle}")).into())
}

fn last_line_position(lines: &[String], needle: &str) -> Result<usize, Box<dyn Error>> {
    lines
        .iter()
        .rposition(|line| line.contains(needle))
        .ok_or_else(|| io::Error::other(format!("output omitted {needle}")).into())
}

/// S10: the interactive client keeps its follow connection live
/// while a second connection approves a streamed tool proposal, then presents
/// the result and continuation delta before the durable terminal reply.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn chat_streams_and_approves_one_scripted_tool_turn() -> Result<(), Box<dyn Error>> {
    let fixture = RunningChatFixture::start().await?;
    let session_id = fixture.create_session().await?;
    let mut child = Command::new(test_bin_path!("signalbox"))
        .kill_on_drop(true)
        .env_remove("SIGNALBOX_SOCKET_PATH")
        .arg("--socket")
        .arg(fixture.socket_directory.socket())
        .arg("chat")
        .arg(&session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("chat stdin was not piped"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("chat stdout was not piped"))?;
    let mut errors = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("chat stderr was not piped"))?;
    let mut lines = BufReader::new(output).lines();
    let mut rendered = Vec::new();

    read_through(&mut lines, &mut rendered, "state=ready").await?;
    input.write_all(APPROVAL_INPUT_LINE.as_bytes()).await?;
    let awaiting = read_through(&mut lines, &mut rendered, "state=awaiting_approval").await?;
    let request = awaiting
        .split_once(" request=")
        .ok_or_else(|| io::Error::other("approval state omitted its request"))?
        .1;
    Uuid::parse_str(request)?;
    input.write_all(STOP_INPUT_LINE.as_bytes()).await?;
    input
        .write_all(format!(":approve {request}\n").as_bytes())
        .await?;
    read_through(&mut lines, &mut rendered, "decision=approve").await?;
    read_through(&mut lines, &mut rendered, FINAL_REPLY).await?;
    read_through(&mut lines, &mut rendered, "state=ready").await?;
    input.write_all(b":quit\n").await?;
    drop(input);

    let status = timeout(Duration::from_secs(20), child.wait()).await??;
    let mut stderr = String::new();
    errors.read_to_string(&mut stderr).await?;
    assert!(status.success(), "chat failed: {stderr}");
    assert!(stderr.contains("command_id="));
    assert!(stderr.contains("defaults_version="));
    assert!(stderr.contains("no turn is queued or running"));
    assert!(stderr.contains("decide it before stopping"));
    assert!(
        line_position(&rendered, "provider_text_delta")?
            < line_position(&rendered, "assistant_tool_use")?
    );
    assert!(
        line_position(&rendered, "assistant_tool_use")?
            < line_position(&rendered, "state=awaiting_approval")?
    );
    assert!(
        line_position(&rendered, "decision=approve")?
            < line_position(&rendered, "tool_execution_result")?
    );
    assert!(
        line_position(&rendered, "tool_execution_result")?
            < last_line_position(&rendered, FINAL_REPLY)?
    );
    assert!(line_position(&rendered, FINAL_REPLY)? < last_line_position(&rendered, FINAL_REPLY)?);
    assert!(
        last_line_position(&rendered, FINAL_REPLY)? < last_line_position(&rendered, "state=ready")?
    );

    fixture.stop().await
}

/// S07: the interactive loop keeps accepted work queued until the
/// durable activation event. Its independent request path then steers that
/// exact active turn before `:stop` atomically cancels it and admits exact
/// successor content without closing the follow connection.
///
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn chat_steers_then_stops_one_active_turn() -> Result<(), Box<dyn Error>> {
    let fixture = RunningIdleFixture::start().await?;
    let session_id = fixture.create_session().await?;
    let mut child = Command::new(test_bin_path!("signalbox"))
        .kill_on_drop(true)
        .env_remove("SIGNALBOX_SOCKET_PATH")
        .arg("--socket")
        .arg(fixture.socket_directory.socket())
        .arg("chat")
        .arg(&session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("chat stdin was not piped"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("chat stdout was not piped"))?;
    let mut errors = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("chat stderr was not piped"))?;
    let mut lines = BufReader::new(output).lines();
    let mut rendered = Vec::new();

    read_through(&mut lines, &mut rendered, "state=ready").await?;
    input.write_all(INITIAL_INPUT_LINE.as_bytes()).await?;
    let queued = read_through(&mut lines, &mut rendered, "state=queued turn=").await?;
    let stopped_turn = queued
        .split_once(" turn=")
        .ok_or_else(|| io::Error::other("queued state omitted its turn"))?
        .1;
    Uuid::parse_str(stopped_turn)?;
    activate_turn(&fixture.pool, Uuid::parse_str(&session_id)?).await?;
    let streaming = read_through(&mut lines, &mut rendered, "state=streaming turn=").await?;
    assert!(streaming.contains(&format!("turn={stopped_turn}")));

    input.write_all(STEERING_INPUT_LINE.as_bytes()).await?;
    let expected_source = format!("source_turn={stopped_turn}");
    let steering = read_through(&mut lines, &mut rendered, &expected_source).await?;
    assert!(steering.contains("accepted_input="));

    input.write_all(STOP_INPUT_LINE.as_bytes()).await?;
    let stopped = read_through(&mut lines, &mut rendered, "stopped_turn=").await?;
    let successor_turn = stopped
        .split_once(" successor_turn=")
        .ok_or_else(|| io::Error::other("stop state omitted its successor"))?
        .1;
    Uuid::parse_str(successor_turn)?;
    input.write_all(b":quit\n").await?;
    drop(input);

    let status = timeout(Duration::from_secs(20), child.wait()).await??;
    let mut stderr = String::new();
    errors.read_to_string(&mut stderr).await?;
    assert!(status.success(), "chat failed: {stderr}");
    assert!(stopped.contains(&format!("stopped_turn={stopped_turn}")));
    assert!(stderr.contains(&format!("turn={stopped_turn}")));
    assert!(stderr.contains(&format!("turn {successor_turn} remains queued")));
    assert!(
        line_position(&rendered, "accepted_input=")? < line_position(&rendered, "stopped_turn=")?
    );
    assert!(
        line_position(&rendered, "state=queued turn=")?
            < line_position(&rendered, "state=streaming turn=")?
    );

    fixture.stop().await
}

/// Ctrl-C remains responsive while the terminal-input worker is blocked on an open
/// stdin pipe: the first interrupt offers the existing stop command and the second
/// exits without cancelling the daemon turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn chat_ctrl_c_exits_with_blocked_stdin_and_active_turn() -> Result<(), Box<dyn Error>> {
    let fixture = RunningIdleFixture::start().await?;
    let session_id = fixture.create_session().await?;
    let mut child = Command::new(test_bin_path!("signalbox"))
        .kill_on_drop(true)
        .env_remove("SIGNALBOX_SOCKET_PATH")
        .arg("--socket")
        .arg(fixture.socket_directory.socket())
        .arg("chat")
        .arg(&session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("chat stdin was not piped"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("chat stdout was not piped"))?;
    let errors = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("chat stderr was not piped"))?;
    let process_id = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::other("chat process omitted a valid process identity"))?;
    let mut lines = BufReader::new(output).lines();
    let mut rendered = Vec::new();
    let mut error_lines = BufReader::new(errors).lines();
    let mut rendered_errors = Vec::new();

    read_through(&mut lines, &mut rendered, "state=ready").await?;
    input.write_all(INITIAL_INPUT_LINE.as_bytes()).await?;
    read_through(&mut lines, &mut rendered, "state=queued turn=").await?;
    activate_turn(&fixture.pool, Uuid::parse_str(&session_id)?).await?;
    read_through(&mut lines, &mut rendered, "state=streaming turn=").await?;
    kill_process(process_id, Signal::INT)?;
    read_through(
        &mut error_lines,
        &mut rendered_errors,
        "press Ctrl-C again to exit leaving it running",
    )
    .await?;
    kill_process(process_id, Signal::INT)?;
    read_through(
        &mut error_lines,
        &mut rendered_errors,
        "remains running in the daemon",
    )
    .await?;

    let status = timeout(Duration::from_secs(20), child.wait()).await??;
    drop(input);
    assert!(status.success(), "chat failed: {rendered_errors:?}");

    fixture.stop().await
}
