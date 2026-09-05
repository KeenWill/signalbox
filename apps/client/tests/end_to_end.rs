#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

mod support;

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, fs,
    io::{self, ErrorKind},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    InProcessAttemptDispatchGate, InProcessEligibilityWorkSource, InProcessToolDispatchGate,
    LoadSessionService, ModelCallCredentialReference, OperatorFailureClass, SchedulerLoop,
    SchedulerLoopExit, StartEligibleTurnOutcome, StartEligibleTurnService, ToolDefinition,
    ToolExecutionInvocation, ToolExecutor, ToolExecutorEvidence, ToolInputSchema,
    UuidV7StartEligibleTurnIdGenerator,
};
use signalbox_domain::{
    DirectModelSelection, ModelTargetCatalog, ModelTargetDefinition, NormalizedToolArguments,
    ProviderModelIdentity, ResolvedProviderTarget, SessionId, SessionTemplateName, ToolEffectClass,
    ToolExecutionErrorDetail, ToolName, ToolPermissionDefault,
};
use signalbox_model_provider_runtime::{
    RuntimeModelCallProvider, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, CredentialReference, ExchangeFacts,
    ProviderReportedModel, Script, ScriptedModel, TerminalEvidence, TokenUsage, ToolCallId,
    ToolCallProposal as RuntimeToolCallProposal, ToolName as RuntimeToolName,
};
use signalbox_model_runtime_anthropic::{AnthropicConfig, AnthropicRuntime};
use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository, scheduler::PostgresEligibilitySweep,
    session::SessionRepository, start_eligible_turn::StartEligibleTurnRepository,
};
use signalbox_process_protocol::{
    CanonicalUuid, ClientFrame, ClientRequest, CommandId, ProtocolVersion, RequestId,
    ServerMessage, SessionMetadata, decode_server_line, encode_client_line,
};
use signalbox_test_bin::test_bin_path;
use signalboxd::{
    ActivatedTurnExecution, ActivatedTurnPass, FatalExecutionSupervisor, FileCredentialAccess,
    HubModelConfiguration, LocalProcessListener, ModelAdapter, PostgresProviderModelExecution,
    ProcessRuntime, ProcessRuntimeError, SessionTemplateConfiguration,
    WorkspaceInstructionPreparedExecution, WorkspaceInstructionRuntime,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::Command,
    sync::watch,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_terminal_client";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
/// A synthetic source larger than one complete import request can carry.
const MULTIFRAME_IMPORT_BYTES: u64 = 8 * 1024 * 1024;
const IMPORT_MODEL_CONFIGURATION: &str = r#"
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
provider_model = "import-fixture"
max_output_tokens = 64
context_window_tokens = 200000

[[models]]
selection_id = "00000000-0000-0000-0000-000000000003"
target_id = "00000000-0000-0000-0000-000000000004"
model_family = "anthropic"
provider_model = "import-fixture-next"
max_output_tokens = 64
context_window_tokens = 200000
"#;

async fn postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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
    Ok((container, pool))
}

struct SocketDirectory {
    directory: PathBuf,
    socket: PathBuf,
}

impl SocketDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let directory = PathBuf::from("/tmp").join(format!("signalbox-client-{}", Uuid::now_v7()));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let socket = directory.join("hub.sock");
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

async fn run_client(
    socket: PathBuf,
    arguments: Vec<String>,
    input: Option<String>,
) -> Result<Output, Box<dyn Error>> {
    let child = spawn_client(socket, arguments, input)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(child.wait_with_output().await?)
}

async fn spawn_client(
    socket: PathBuf,
    arguments: Vec<String>,
    input: Option<String>,
) -> Result<tokio::process::Child, Box<dyn Error>> {
    let mut command = Command::new(test_bin_path!("signalbox"));
    command
        .kill_on_drop(true)
        .env_remove("SIGNALBOX_SOCKET_PATH")
        .arg("--socket")
        .arg(socket)
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command.spawn()?;
    if let Some(input) = input {
        let mut child_input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(ErrorKind::BrokenPipe, "client stdin was not piped"))?;
        child_input.write_all(input.as_bytes()).await?;
    }
    Ok(child)
}

fn require_activated_turn(
    outcome: StartEligibleTurnOutcome,
) -> Result<Box<signalbox_domain::ActivatedTurn>, io::Error> {
    match outcome {
        StartEligibleTurnOutcome::Activated(activated) => Ok(activated),
        StartEligibleTurnOutcome::NoEligibleTurn => Err(io::Error::other(
            "the accepted input did not produce an eligible turn",
        )),
    }
}

/// The direct model selection every metadata-search fixture session carries:
/// the first selection `IMPORT_MODEL_CONFIGURATION` defines, since the search
/// verb reads metadata and never depends on which model a session selected.
const SEARCH_FIXTURE_SELECTION: &str = "00000000-0000-0000-0000-000000000001";

/// The process server the metadata-search tests drive. They start no turn, so
/// the fixture runs the process boundary without a scheduler or provider.
/// One independently restartable process runtime sharing a caller-owned pool.
struct TemplateProcessRuntime {
    socket_directory: SocketDirectory,
    shutdown: watch::Sender<bool>,
    process_task: JoinHandle<Result<(), ProcessRuntimeError>>,
    _work_source: InProcessEligibilityWorkSource<PostgresEligibilitySweep>,
}

impl TemplateProcessRuntime {
    async fn start(
        pool: &PgPool,
        models: HubModelConfiguration,
        templates: SessionTemplateConfiguration,
    ) -> Result<Self, Box<dyn Error>> {
        let socket_directory = SocketDirectory::create()?;
        let sweep = PostgresEligibilitySweep::new(pool.clone());
        let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
        let listener = LocalProcessListener::bind(socket_directory.socket())?;
        let process_runtime = ProcessRuntime::new_with_templates(
            listener,
            pool.clone(),
            eligibility_nudge,
            InProcessToolDispatchGate::default(),
            models,
            templates,
        );
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let process_task = tokio::spawn(process_runtime.run(shutdown_receiver));
        Ok(Self {
            socket_directory,
            shutdown,
            process_task,
            _work_source: work_source,
        })
    }

    fn socket(&self) -> PathBuf {
        self.socket_directory.socket().to_owned()
    }

    async fn stop(self) -> Result<(), Box<dyn Error>> {
        self.shutdown.send(true)?;
        timeout(Duration::from_secs(10), self.process_task).await???;
        self.socket_directory.cleanup()
    }
}

struct MetadataSearchRuntime {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    socket_directory: SocketDirectory,
    shutdown: watch::Sender<bool>,
    process_task: JoinHandle<Result<(), ProcessRuntimeError>>,
    _work_source: InProcessEligibilityWorkSource<PostgresEligibilitySweep>,
}

impl MetadataSearchRuntime {
    async fn start() -> Result<Self, Box<dyn Error>> {
        let (container, pool) = postgres().await?;
        let socket_directory = SocketDirectory::create()?;
        let sweep = PostgresEligibilitySweep::new(pool.clone());
        let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
        let listener = LocalProcessListener::bind(socket_directory.socket())?;
        let process_runtime = ProcessRuntime::new(
            listener,
            pool.clone(),
            eligibility_nudge,
            InProcessToolDispatchGate::default(),
            support::parse_model_configuration(IMPORT_MODEL_CONFIGURATION)?,
        );
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let process_task = tokio::spawn(process_runtime.run(shutdown_receiver));
        Ok(Self {
            container,
            pool,
            socket_directory,
            shutdown,
            process_task,
            _work_source: work_source,
        })
    }

    fn socket(&self) -> PathBuf {
        self.socket_directory.socket().to_owned()
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

/// Creates one fixture session through the shipped terminal verb and returns
/// its canonical identity text.
async fn create_fixture_session(socket: PathBuf) -> Result<String, Box<dyn Error>> {
    let created = run_client(
        socket,
        vec![
            String::from("create"),
            String::from("--model"),
            String::from(SEARCH_FIXTURE_SELECTION),
        ],
        None,
    )
    .await?;
    assert!(
        created.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let session_id = String::from_utf8(created.stdout)?.trim().to_owned();
    Uuid::parse_str(&session_id)?;
    Ok(session_id)
}

/// Installs one complete metadata snapshot through the process request, which no terminal verb exposes.
async fn replace_fixture_metadata(
    socket: &Path,
    session_id: &str,
    metadata: SessionMetadata,
) -> Result<(), Box<dyn Error>> {
    let stream = UnixStream::connect(socket).await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        RequestId::try_new(1)?,
        ClientRequest::ReplaceSessionMetadata {
            command_id: CommandId::try_from_uuid(Uuid::now_v7())?,
            session_id: CanonicalUuid::from_uuid(Uuid::parse_str(session_id)?),
            metadata,
        },
    )?;
    writer.write_all(&encode_client_line(&frame)?).await?;
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).await?;
    let response = decode_server_line(&line)?;
    assert!(
        matches!(
            response.message(),
            ServerMessage::SessionMetadataReplaced { .. }
        ),
        "the metadata fixture must commit: {:?}",
        response.message()
    );
    Ok(())
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

fn required_environment(name: &'static str) -> Result<OsString, Box<dyn Error>> {
    env::var_os(name).ok_or_else(|| {
        io::Error::new(
            ErrorKind::NotFound,
            format!("the ignored smoke test requires {name}"),
        )
        .into()
    })
}

fn required_canonical_uuid_environment(name: &'static str) -> Result<Uuid, Box<dyn Error>> {
    let text = required_environment(name)?.into_string().map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("{name} must be valid UTF-8"),
        )
    })?;
    let uuid = Uuid::parse_str(&text)?;
    if uuid.hyphenated().to_string() != text {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("{name} must be canonical lowercase UUID text"),
        )
        .into());
    }
    Ok(uuid)
}

/// S35: the shipped client lists daemon-owned templates, creates from one
/// resolved startup snapshot, and a catalog edit plus daemon reload changes
/// only later sessions while both copies retain exact provenance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s35_terminal_template_create_is_copy_on_create_across_daemon_reload()
-> Result<(), Box<dyn Error>> {
    const TEMPLATE_NAME: &str = "reviewer";
    const ORIGINAL_TEMPLATE_VERSION: u64 = 1;
    const EDITED_TEMPLATE_VERSION: u64 = 2;
    const ORIGINAL_PROMPT: &str = "Review the change and report concrete findings.";
    const EDITED_PROMPT: &str = "Review the change and prioritize correctness findings.";
    const ORIGINAL_COMMAND: &str = "40000000-0000-4000-8000-000000000001";
    const EDITED_COMMAND: &str = "40000000-0000-4000-8000-000000000002";

    let (container, pool) = postgres().await?;
    let deployment = tempfile::tempdir()?;
    let catalog_path = deployment.path().join("session-templates.toml");
    let models = support::parse_model_configuration(IMPORT_MODEL_CONFIGURATION)?;
    let original_catalog = format!(
        r#"
version = 1

[[templates]]
name = "{TEMPLATE_NAME}"
version = {ORIGINAL_TEMPLATE_VERSION}
model = "00000000-0000-0000-0000-000000000001"
system_prompt = "{ORIGINAL_PROMPT}"
dangerous_tool_auto_approval = true
"#,
    );
    fs::write(&catalog_path, original_catalog)?;
    let original_templates = SessionTemplateConfiguration::read(&catalog_path, || None, &models)?;
    let template_name = SessionTemplateName::try_new(TEMPLATE_NAME.to_owned())?;
    let original_template = original_templates
        .resolve(&template_name)
        .expect("original template resolves");
    let original_provenance = original_template.provenance().clone();
    let original_defaults = original_template.defaults().clone();
    let original_runtime =
        TemplateProcessRuntime::start(&pool, models.clone(), original_templates).await?;

    let original_list = run_client(
        original_runtime.socket(),
        vec![String::from("templates")],
        None,
    )
    .await?;
    assert!(original_list.status.success());
    assert_eq!(
        String::from_utf8(original_list.stdout)?,
        format!("name={TEMPLATE_NAME} version={ORIGINAL_TEMPLATE_VERSION}\n")
    );
    let original_create = run_client(
        original_runtime.socket(),
        vec![
            String::from("create"),
            String::from("--template"),
            String::from(TEMPLATE_NAME),
            String::from("--command-id"),
            String::from(ORIGINAL_COMMAND),
        ],
        None,
    )
    .await?;
    assert!(
        original_create.status.success(),
        "template create failed: {}",
        String::from_utf8_lossy(&original_create.stderr)
    );
    assert!(original_create.stderr.is_empty());
    let original_session = Uuid::parse_str(String::from_utf8(original_create.stdout)?.trim())?;
    original_runtime.stop().await?;

    let edited_catalog = format!(
        r#"
version = 1

[[templates]]
name = "{TEMPLATE_NAME}"
version = {EDITED_TEMPLATE_VERSION}
model = "00000000-0000-0000-0000-000000000003"
system_prompt = "{EDITED_PROMPT}"
dangerous_tool_auto_approval = false
"#,
    );
    fs::write(&catalog_path, edited_catalog)?;
    let edited_templates = SessionTemplateConfiguration::read(&catalog_path, || None, &models)?;
    let edited_template = edited_templates
        .resolve(&template_name)
        .expect("edited template resolves");
    let edited_provenance = edited_template.provenance().clone();
    let edited_defaults = edited_template.defaults().clone();
    let edited_runtime =
        TemplateProcessRuntime::start(&pool, models.clone(), edited_templates).await?;
    let edited_list = run_client(
        edited_runtime.socket(),
        vec![String::from("templates")],
        None,
    )
    .await?;
    assert!(edited_list.status.success());
    assert_eq!(
        String::from_utf8(edited_list.stdout)?,
        format!("name={TEMPLATE_NAME} version={EDITED_TEMPLATE_VERSION}\n")
    );
    let edited_create = run_client(
        edited_runtime.socket(),
        vec![
            String::from("create"),
            String::from("--template"),
            String::from(TEMPLATE_NAME),
            String::from("--command-id"),
            String::from(EDITED_COMMAND),
        ],
        None,
    )
    .await?;
    assert!(edited_create.status.success());
    assert!(edited_create.stderr.is_empty());
    let edited_session = Uuid::parse_str(String::from_utf8(edited_create.stdout)?.trim())?;
    edited_runtime.stop().await?;

    fs::write(&catalog_path, "version = 1\n")?;
    let empty_templates = SessionTemplateConfiguration::read(&catalog_path, || None, &models)?;
    let replay_runtime =
        TemplateProcessRuntime::start(&pool, models.clone(), empty_templates).await?;
    let original_replay = run_client(
        replay_runtime.socket(),
        vec![
            String::from("create"),
            String::from("--template"),
            String::from(TEMPLATE_NAME),
            String::from("--command-id"),
            String::from(ORIGINAL_COMMAND),
        ],
        None,
    )
    .await?;
    assert!(
        original_replay.status.success(),
        "template replay after removal failed: {}",
        String::from_utf8_lossy(&original_replay.stderr)
    );
    assert!(original_replay.stderr.is_empty());
    assert_eq!(
        Uuid::parse_str(String::from_utf8(original_replay.stdout)?.trim())?,
        original_session
    );
    replay_runtime.stop().await?;

    let load = LoadSessionService::new(SessionRepository::new(pool.clone()));
    let original_loaded = load
        .execute(SessionId::from_uuid(original_session))
        .await?
        .expect("original template session remains loadable after reload");
    let edited_loaded = load
        .execute(SessionId::from_uuid(edited_session))
        .await?
        .expect("edited template session is loadable");

    assert_eq!(
        original_loaded.current_configuration_defaults().defaults(),
        &original_defaults
    );
    assert_eq!(
        original_loaded.template_provenance(),
        Some(&original_provenance)
    );
    assert_eq!(
        edited_loaded.current_configuration_defaults().defaults(),
        &edited_defaults
    );
    assert_eq!(
        edited_loaded.template_provenance(),
        Some(&edited_provenance)
    );
    assert_ne!(original_provenance, edited_provenance);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S25: the terminal search verb lists only the sessions that satisfy every
/// named filter, excluding one that fails the title query and one that fails
/// the required tag.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s25_terminal_client_search_lists_only_sessions_matching_every_filter()
-> Result<(), Box<dyn Error>> {
    let runtime = MetadataSearchRuntime::start().await?;
    let matching_session = create_fixture_session(runtime.socket()).await?;
    let other_title_session = create_fixture_session(runtime.socket()).await?;
    let other_tag_session = create_fixture_session(runtime.socket()).await?;
    replace_fixture_metadata(
        &runtime.socket(),
        &matching_session,
        SessionMetadata::try_new(
            Some(String::from("Active plan")),
            vec![String::from("daily"), String::from("plan")],
            Vec::new(),
            false,
        )?,
    )
    .await?;
    replace_fixture_metadata(
        &runtime.socket(),
        &other_title_session,
        SessionMetadata::try_new(
            Some(String::from("Retired plan")),
            vec![String::from("daily")],
            Vec::new(),
            false,
        )?,
    )
    .await?;
    replace_fixture_metadata(
        &runtime.socket(),
        &other_tag_session,
        SessionMetadata::try_new(
            Some(String::from("Active plan")),
            vec![String::from("weekly")],
            Vec::new(),
            false,
        )?,
    )
    .await?;

    let searched = run_client(
        runtime.socket(),
        vec![
            String::from("search"),
            String::from("--title"),
            String::from("Active"),
            String::from("--tag"),
            String::from("daily"),
        ],
        None,
    )
    .await?;

    assert!(
        searched.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&searched.stderr)
    );
    assert!(searched.stderr.is_empty());
    let listed = String::from_utf8(searched.stdout)?;
    assert_eq!(listed.lines().count(), 1);
    assert!(listed.contains(&format!(
        "{matching_session} archived=false defaults_version=1 \
         model={SEARCH_FIXTURE_SELECTION} dangerous_tool_auto_approval=disabled \
         last_writer=user updated_at_unix_micros="
    )));
    assert!(listed.contains(" tags=daily,plan title=Active plan\n"));
    assert!(!listed.contains(&other_title_session));
    assert!(!listed.contains(&other_tag_session));

    runtime.stop().await
}

/// S25: archiving removes a session from the default search view, and the
/// explicit switch restores it while naming its archive state.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s25_terminal_client_search_lists_an_archived_session_only_when_requested()
-> Result<(), Box<dyn Error>> {
    let runtime = MetadataSearchRuntime::start().await?;
    let active_session = create_fixture_session(runtime.socket()).await?;
    let archived_session = create_fixture_session(runtime.socket()).await?;
    replace_fixture_metadata(
        &runtime.socket(),
        &active_session,
        SessionMetadata::try_new(
            Some(String::from("Active plan")),
            vec![String::from("daily")],
            Vec::new(),
            false,
        )?,
    )
    .await?;
    replace_fixture_metadata(
        &runtime.socket(),
        &archived_session,
        SessionMetadata::try_new(
            Some(String::from("Archived plan")),
            vec![String::from("daily")],
            Vec::new(),
            true,
        )?,
    )
    .await?;

    let default_view = run_client(
        runtime.socket(),
        vec![
            String::from("search"),
            String::from("--tag"),
            String::from("daily"),
        ],
        None,
    )
    .await?;
    assert!(
        default_view.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&default_view.stderr)
    );
    assert!(default_view.stderr.is_empty());
    let default_listed = String::from_utf8(default_view.stdout)?;
    assert_eq!(default_listed.lines().count(), 1);
    assert!(default_listed.contains(&active_session));
    assert!(!default_listed.contains(&archived_session));

    let archived_view = run_client(
        runtime.socket(),
        vec![
            String::from("search"),
            String::from("--tag"),
            String::from("daily"),
            String::from("--include-archived"),
        ],
        None,
    )
    .await?;
    assert!(
        archived_view.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&archived_view.stderr)
    );
    assert!(archived_view.stderr.is_empty());
    let archived_listed = String::from_utf8(archived_view.stdout)?;
    assert_eq!(archived_listed.lines().count(), 2);
    assert!(archived_listed.contains(&active_session));
    assert!(archived_listed.contains(&format!("{archived_session} archived=true")));

    runtime.stop().await
}

/// A bounded page never truncates silently: a page that reached its limit
/// prints the exact cursor that continues it, and the next page carries none.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn terminal_client_search_prints_the_cursor_that_continues_a_full_page()
-> Result<(), Box<dyn Error>> {
    let runtime = MetadataSearchRuntime::start().await?;
    let first_created = create_fixture_session(runtime.socket()).await?;
    let second_created = create_fixture_session(runtime.socket()).await?;

    let first_page = run_client(
        runtime.socket(),
        vec![
            String::from("search"),
            String::from("--limit"),
            String::from("1"),
        ],
        None,
    )
    .await?;
    assert!(
        first_page.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&first_page.stderr)
    );
    let first_listed = String::from_utf8(first_page.stdout)?;
    assert_eq!(first_listed.lines().count(), 1);
    let first_page_session = first_listed
        .split(' ')
        .next()
        .expect("each row begins with its session identity")
        .to_owned();
    assert_eq!(
        String::from_utf8(first_page.stderr)?,
        format!("next_after_session_id={first_page_session}\n")
    );

    let second_page = run_client(
        runtime.socket(),
        vec![
            String::from("search"),
            String::from("--limit"),
            String::from("1"),
            String::from("--after"),
            first_page_session.clone(),
        ],
        None,
    )
    .await?;
    assert!(
        second_page.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&second_page.stderr)
    );
    assert!(second_page.stderr.is_empty());
    let second_listed = String::from_utf8(second_page.stdout)?;
    assert_eq!(second_listed.lines().count(), 1);
    assert!(!second_listed.contains(&first_page_session));

    let both_pages = format!("{first_listed}{second_listed}");
    assert!(both_pages.contains(&first_created));
    assert!(both_pages.contains(&second_created));

    runtime.stop().await
}

/// One synthetic Claude Code export whose summary record supplies the derived
/// display title the unified listing presents.
const CONVERSATIONS_IMPORT_SOURCE: &str = concat!(
    "{\"type\":\"summary\",\"summary\":\"Imported planning summary\"}\n",
    "{\"sessionId\":\"terminal-conversations\",\"type\":\"user\",",
    "\"message\":{\"role\":\"user\",\"content\":\"imported question\"}}"
);
/// The exact title [`CONVERSATIONS_IMPORT_SOURCE`]'s summary record derives.
const CONVERSATIONS_IMPORT_TITLE: &str = "Imported planning summary";
/// The normalized entry count of [`CONVERSATIONS_IMPORT_SOURCE`], which is
/// also the greatest `--through-position` a continuation may select.
const CONVERSATIONS_IMPORT_ENTRY_COUNT: &str = "2";

/// Imports [`CONVERSATIONS_IMPORT_SOURCE`] through the shipped import verb
/// and returns the inserted imported-conversation identity text.
async fn import_fixture_conversation(socket: PathBuf) -> Result<String, Box<dyn Error>> {
    let source_directory = tempfile::tempdir()?;
    let source_path = source_directory.path().join("conversations-fixture.jsonl");
    fs::write(&source_path, CONVERSATIONS_IMPORT_SOURCE)?;
    let imported = run_client(
        socket,
        vec![
            String::from("import"),
            String::from("--format"),
            String::from("claude-code"),
            source_path.display().to_string(),
        ],
        None,
    )
    .await?;
    assert!(
        imported.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let imported_output = String::from_utf8(imported.stdout)?;
    let imported_identity = imported_output
        .strip_prefix("inserted imported_conversation_id=")
        .expect("the fixture import carries the inserted outcome")
        .trim()
        .to_owned();
    Uuid::parse_str(&imported_identity)?;
    Ok(imported_identity)
}

/// The unified listing presents one origin-tagged line per conversation:
/// native sessions with their organizational facts and imported conversations
/// with their derived title, entry count, and source format.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn terminal_client_conversations_lists_native_and_imported_rows() -> Result<(), Box<dyn Error>>
{
    let runtime = MetadataSearchRuntime::start().await?;
    let native_session = create_fixture_session(runtime.socket()).await?;
    let imported_identity = import_fixture_conversation(runtime.socket()).await?;

    let listed = run_client(runtime.socket(), vec![String::from("conversations")], None).await?;

    assert!(
        listed.status.success(),
        "conversations failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(listed.stderr.is_empty());
    let output = String::from_utf8(listed.stdout)?;
    assert_eq!(output.lines().count(), 2);
    assert!(output.contains(&format!(
        "origin=native session_id={native_session} archived=false defaults_version=1 title=\n"
    )));
    assert!(output.contains(&format!(
        "origin=imported imported_conversation_id={imported_identity} \
         format=claude-code-session-jsonl-v2 \
         entry_count={CONVERSATIONS_IMPORT_ENTRY_COUNT} \
         title={CONVERSATIONS_IMPORT_TITLE}\n"
    )));

    runtime.stop().await
}

/// A full unified page prints its origin-qualified continuation cursor to
/// standard error, and the next invocation resumes exactly after it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn terminal_client_conversations_prints_the_cursor_that_continues_a_full_page()
-> Result<(), Box<dyn Error>> {
    let runtime = MetadataSearchRuntime::start().await?;
    let native_session = create_fixture_session(runtime.socket()).await?;
    let imported_identity = import_fixture_conversation(runtime.socket()).await?;

    let first_page = run_client(
        runtime.socket(),
        vec![
            String::from("conversations"),
            String::from("--limit"),
            String::from("1"),
        ],
        None,
    )
    .await?;
    assert!(
        first_page.status.success(),
        "conversations failed: {}",
        String::from_utf8_lossy(&first_page.stderr)
    );
    let first_listed = String::from_utf8(first_page.stdout)?;
    assert_eq!(first_listed.lines().count(), 1);
    let cursor = String::from_utf8(first_page.stderr)?
        .strip_prefix("next_after=")
        .expect("a full page prints its continuation cursor")
        .trim()
        .to_owned();

    let second_page = run_client(
        runtime.socket(),
        vec![
            String::from("conversations"),
            String::from("--limit"),
            String::from("1"),
            String::from("--after"),
            cursor,
        ],
        None,
    )
    .await?;
    assert!(
        second_page.status.success(),
        "conversations failed: {}",
        String::from_utf8_lossy(&second_page.stderr)
    );
    assert!(second_page.stderr.is_empty());
    let second_listed = String::from_utf8(second_page.stdout)?;
    assert_eq!(second_listed.lines().count(), 1);

    let both_pages = format!("{first_listed}{second_listed}");
    assert!(both_pages.contains(&native_session));
    assert!(both_pages.contains(&imported_identity));

    runtime.stop().await
}

/// A listed imported row names exactly what `continue` requires — its
/// identity and greatest position — and the continuation's live session then
/// appears among the native rows of the same unified surface.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn terminal_client_conversations_imported_row_feeds_continue() -> Result<(), Box<dyn Error>> {
    let runtime = MetadataSearchRuntime::start().await?;
    let imported_identity = import_fixture_conversation(runtime.socket()).await?;

    let imported_only = run_client(
        runtime.socket(),
        vec![
            String::from("conversations"),
            String::from("--origin"),
            String::from("imported"),
        ],
        None,
    )
    .await?;
    assert!(
        imported_only.status.success(),
        "conversations failed: {}",
        String::from_utf8_lossy(&imported_only.stderr)
    );
    let imported_listed = String::from_utf8(imported_only.stdout)?;
    assert_eq!(
        imported_listed,
        format!(
            "origin=imported imported_conversation_id={imported_identity} \
             format=claude-code-session-jsonl-v2 \
             entry_count={CONVERSATIONS_IMPORT_ENTRY_COUNT} \
             title={CONVERSATIONS_IMPORT_TITLE}\n"
        )
    );

    let continued = run_client(
        runtime.socket(),
        vec![
            String::from("continue"),
            imported_identity,
            String::from("--through-position"),
            String::from(CONVERSATIONS_IMPORT_ENTRY_COUNT),
            String::from("--relationship"),
            String::from("resume"),
            String::from("--model"),
            String::from(SEARCH_FIXTURE_SELECTION),
        ],
        None,
    )
    .await?;
    assert!(
        continued.status.success(),
        "continue failed: {}",
        String::from_utf8_lossy(&continued.stderr)
    );
    let continued_session = String::from_utf8(continued.stdout)?.trim().to_owned();
    Uuid::parse_str(&continued_session)?;

    let native_only = run_client(
        runtime.socket(),
        vec![
            String::from("conversations"),
            String::from("--origin"),
            String::from("native"),
        ],
        None,
    )
    .await?;
    assert!(
        native_only.status.success(),
        "conversations failed: {}",
        String::from_utf8_lossy(&native_only.stderr)
    );
    let native_listed = String::from_utf8(native_only.stdout)?;
    assert_eq!(native_listed.lines().count(), 1);
    assert!(native_listed.contains(&continued_session));

    runtime.stop().await
}

/// S28: the shipped terminal verb reads one named file and exposes
/// first insertion separately from exact-snapshot reimport.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn terminal_client_imports_one_file_and_reports_exact_reimport() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let source_directory = tempfile::tempdir()?;
    let source_path = source_directory.path().join("session.jsonl");
    fs::write(
        &source_path,
        concat!(
            "{\"sessionId\":\"terminal-import\",\"type\":\"user\",",
            "\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"sessionId\":\"terminal-import\",\"type\":\"assistant\",",
            "\"message\":{\"role\":\"assistant\",\"content\":\"answer\"}}"
        ),
    )?;
    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (eligibility_nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let listener = LocalProcessListener::bind(socket_directory.socket())?;
    let process_runtime = ProcessRuntime::new(
        listener,
        pool.clone(),
        eligibility_nudge,
        InProcessToolDispatchGate::default(),
        support::parse_model_configuration(IMPORT_MODEL_CONFIGURATION)?,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let process_task = tokio::spawn(process_runtime.run(shutdown_receiver));
    let arguments = vec![
        String::from("import"),
        String::from("--format"),
        String::from("claude-code"),
        source_path.display().to_string(),
    ];

    let inserted = run_client(
        socket_directory.socket().to_owned(),
        arguments.clone(),
        None,
    )
    .await?;
    assert!(inserted.status.success());
    assert!(inserted.stderr.is_empty());
    let inserted_output = String::from_utf8(inserted.stdout)?;
    let inserted_identity = inserted_output
        .strip_prefix("inserted imported_conversation_id=")
        .expect("the first receipt carries the inserted outcome")
        .trim();
    Uuid::parse_str(inserted_identity)?;

    let already_imported =
        run_client(socket_directory.socket().to_owned(), arguments, None).await?;
    assert!(already_imported.status.success());
    assert!(already_imported.stderr.is_empty());
    assert_eq!(
        String::from_utf8(already_imported.stdout)?,
        format!("already_imported imported_conversation_id={inserted_identity}\n")
    );

    shutdown.send(true)?;
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}

/// The model selection the imported-continuation test installs on the session
/// it creates. It is the first selection `IMPORT_MODEL_CONFIGURATION` defines;
/// the test starts no turn, so which model it names does not affect the
/// boundary under test.
const IMPORTED_CONTINUATION_SELECTION: &str = "00000000-0000-0000-0000-000000000001";

/// The synthetic imported source both imported-inspection tests read, and the
/// exact entries its two positions carry.
struct ImportedInspectionFixture {
    user_text: &'static str,
    assistant_text: &'static str,
}

impl ImportedInspectionFixture {
    fn new() -> Self {
        Self {
            user_text: "synthetic imported question",
            assistant_text: "synthetic imported answer",
        }
    }

    /// The greatest selectable position, which is also the entry count: the
    /// two-record source below emits exactly one entry per record.
    fn last_position(&self) -> u64 {
        2
    }

    /// One row per selectable position plus the trailing count line.
    fn listed_line_count(&self) -> usize {
        usize::try_from(self.last_position()).expect("the fixture position fits a line count") + 1
    }

    fn source(&self) -> String {
        format!(
            "{{\"sessionId\":\"terminal-import-inspect\",\"type\":\"user\",\
             \"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}\n\
             {{\"sessionId\":\"terminal-import-inspect\",\"type\":\"assistant\",\
             \"message\":{{\"role\":\"assistant\",\"content\":\"{}\"}}}}",
            self.user_text, self.assistant_text,
        )
    }
}

/// S28: the shipped terminal exposes an imported conversation's selectable
/// positions with their previews and total, so the position `continue`
/// consumes never has to be guessed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_terminal_client_completes_an_offline_imported_inspection() -> Result<(), Box<dyn Error>>
{
    let fixture = ImportedInspectionFixture::new();
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let source_directory = tempfile::tempdir()?;
    let source_path = source_directory.path().join("inspect-session.jsonl");
    fs::write(&source_path, fixture.source())?;
    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (eligibility_nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let listener = LocalProcessListener::bind(socket_directory.socket())?;
    let process_runtime = ProcessRuntime::new(
        listener,
        pool.clone(),
        eligibility_nudge,
        InProcessToolDispatchGate::default(),
        support::parse_model_configuration(IMPORT_MODEL_CONFIGURATION)?,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let process_task = tokio::spawn(process_runtime.run(shutdown_receiver));

    let imported_conversation_id =
        import_inspection_source(socket_directory.socket(), &source_path).await?;
    let listed = run_client(
        socket_directory.socket().to_owned(),
        vec![String::from("imported"), imported_conversation_id],
        None,
    )
    .await?;

    assert!(
        listed.status.success(),
        "imported failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(listed.stderr.is_empty());
    let rows: Vec<String> = String::from_utf8(listed.stdout)?
        .lines()
        .map(str::to_owned)
        .collect();
    let first = rows
        .first()
        .expect("the first selectable position is listed");
    assert!(first.starts_with("position=1 "), "first row: {first}");
    assert!(first.contains(" speaker=user kind=text truncated=false text="));
    assert!(first.ends_with(fixture.user_text), "first row: {first}");
    let second = rows
        .get(1)
        .expect("the second selectable position is listed");
    assert!(
        second.starts_with(&format!("position={} ", fixture.last_position())),
        "second row: {second}"
    );
    assert!(second.contains(" speaker=assistant kind=text truncated=false text="));
    assert!(
        second.ends_with(fixture.assistant_text),
        "second row: {second}"
    );
    assert_eq!(
        rows.get(2).map(String::as_str),
        Some(format!("entry_count={}", fixture.last_position()).as_str())
    );
    assert_eq!(rows.len(), fixture.listed_line_count());

    shutdown.send(true)?;
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}

/// S28: `latest` resolves to the imported conversation's final position,
/// prints that concrete ordinal, and seeds the created session through it, so
/// the user never has to know the count to continue from the end.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_terminal_client_completes_an_offline_latest_position_continuation()
-> Result<(), Box<dyn Error>> {
    let fixture = ImportedInspectionFixture::new();
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let source_directory = tempfile::tempdir()?;
    let source_path = source_directory.path().join("latest-session.jsonl");
    fs::write(&source_path, fixture.source())?;
    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (eligibility_nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let listener = LocalProcessListener::bind(socket_directory.socket())?;
    let process_runtime = ProcessRuntime::new(
        listener,
        pool.clone(),
        eligibility_nudge,
        InProcessToolDispatchGate::default(),
        support::parse_model_configuration(IMPORT_MODEL_CONFIGURATION)?,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let process_task = tokio::spawn(process_runtime.run(shutdown_receiver));

    let imported_conversation_id =
        import_inspection_source(socket_directory.socket(), &source_path).await?;
    let continued = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("continue"),
            imported_conversation_id,
            String::from("--through-position"),
            String::from("latest"),
            String::from("--relationship"),
            String::from("resume"),
            String::from("--model"),
            String::from(IMPORTED_CONTINUATION_SELECTION),
        ],
        None,
    )
    .await?;

    assert!(
        continued.status.success(),
        "continue failed: {}",
        String::from_utf8_lossy(&continued.stderr)
    );
    let session_id = String::from_utf8(continued.stdout)?.trim().to_owned();
    Uuid::parse_str(&session_id)?;
    let printed = String::from_utf8(continued.stderr)?;
    assert!(printed.starts_with("command_id="), "printed: {printed}");
    assert!(
        printed.ends_with(&format!("through_position={}\n", fixture.last_position())),
        "printed: {printed}"
    );

    let transcript = run_client(
        socket_directory.socket().to_owned(),
        vec![String::from("transcript"), session_id],
        None,
    )
    .await?;
    assert!(transcript.status.success());
    let transcript = String::from_utf8(transcript.stdout)?;
    assert!(
        transcript.contains(fixture.user_text),
        "transcript: {transcript}"
    );
    assert!(
        transcript.contains(fixture.assistant_text),
        "transcript: {transcript}"
    );

    shutdown.send(true)?;
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}

/// Imports one synthetic inspection source file and returns the durable
/// imported conversation identity its receipt names.
async fn import_inspection_source(
    socket: &Path,
    source_path: &Path,
) -> Result<String, Box<dyn Error>> {
    let imported = run_client(
        socket.to_owned(),
        vec![
            String::from("import"),
            String::from("--format"),
            String::from("claude-code"),
            source_path.display().to_string(),
        ],
        None,
    )
    .await?;
    assert!(
        imported.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let identity = String::from_utf8(imported.stdout)?
        .strip_prefix("inserted imported_conversation_id=")
        .expect("the synthetic import returns an inserted receipt")
        .trim()
        .to_owned();
    Uuid::parse_str(&identity)?;
    Ok(identity)
}

/// S28: scan mode selects recursive matching regular files and
/// reports them in deterministic path order.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_terminal_client_scan_selects_recursive_files_in_sorted_path_order()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let source_directory = tempfile::tempdir()?;
    let first_path = source_directory.path().join("01-session.jsonl");
    let nested_directory = source_directory.path().join("nested");
    let second_path = nested_directory.join("02-session.jsonl");
    fs::create_dir(&nested_directory)?;
    fs::write(
        &first_path,
        concat!(
            "{\"sessionId\":\"terminal-scan-first\",\"type\":\"user\",",
            "\"message\":{\"role\":\"user\",\"content\":\"first question\"}}\n",
            "{\"sessionId\":\"terminal-scan-first\",\"type\":\"assistant\",",
            "\"message\":{\"role\":\"assistant\",\"content\":\"first answer\"}}"
        ),
    )?;
    fs::write(
        &second_path,
        concat!(
            "{\"sessionId\":\"terminal-scan-second\",\"type\":\"user\",",
            "\"message\":{\"role\":\"user\",\"content\":\"second question\"}}\n",
            "{\"sessionId\":\"terminal-scan-second\",\"type\":\"assistant\",",
            "\"message\":{\"role\":\"assistant\",\"content\":\"second answer\"}}"
        ),
    )?;
    fs::write(
        source_directory.path().join("ignored.JSONL"),
        b"not selected",
    )?;
    fs::write(source_directory.path().join("ignored.txt"), b"not selected")?;
    symlink(
        &first_path,
        source_directory.path().join("03-symlink.jsonl"),
    )?;
    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (eligibility_nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let listener = LocalProcessListener::bind(socket_directory.socket())?;
    let process_runtime = ProcessRuntime::new(
        listener,
        pool.clone(),
        eligibility_nudge,
        InProcessToolDispatchGate::default(),
        support::parse_model_configuration(IMPORT_MODEL_CONFIGURATION)?,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let process_task = tokio::spawn(process_runtime.run(shutdown_receiver));
    let arguments = vec![
        String::from("import"),
        String::from("--format"),
        String::from("claude-code"),
        String::from("--scan"),
        source_directory.path().display().to_string(),
    ];

    let inserted = run_client(socket_directory.socket().to_owned(), arguments, None).await?;

    assert!(inserted.status.success());
    assert!(inserted.stderr.is_empty());
    let inserted_output = String::from_utf8(inserted.stdout)?;
    let first_identity = scan_imported_identity(
        inserted_output
            .lines()
            .next()
            .ok_or_else(|| io::Error::other("the first scan outcome is absent"))?,
        &first_path,
    )?;
    let second_identity = scan_imported_identity(
        inserted_output
            .lines()
            .nth(1)
            .ok_or_else(|| io::Error::other("the nested scan outcome is absent"))?,
        &second_path,
    )?;
    assert_eq!(
        inserted_output,
        format!(
            "imported path={:?} imported_conversation_id={first_identity}\n\
             imported path={:?} imported_conversation_id={second_identity}\n\
             scan_summary imported=2 already_imported=0 skipped=0\n",
            first_path, second_path,
        )
    );

    shutdown.send(true)?;
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}

/// S28: scan mode routes a source larger than one frame to the
/// process transport instead of rejecting or truncating it locally.
#[tokio::test]
async fn s28_terminal_client_scan_routes_multiframe_source_to_transport()
-> Result<(), Box<dyn Error>> {
    let source_directory = tempfile::tempdir()?;
    let oversized_path = source_directory.path().join("oversized.jsonl");
    std::fs::File::create(&oversized_path)?.set_len(MULTIFRAME_IMPORT_BYTES)?;
    let arguments = vec![
        String::from("import"),
        String::from("--format"),
        String::from("claude-code"),
        String::from("--scan"),
        source_directory.path().display().to_string(),
    ];

    let skipped = run_client(
        source_directory.path().join("missing.sock"),
        arguments,
        None,
    )
    .await?;

    assert!(!skipped.status.success());
    assert_eq!(
        String::from_utf8(skipped.stdout)?,
        format!(
            concat!(
                "skipped path={:?} reason=local process communication failed\n",
                "scan_summary imported=0 already_imported=0 skipped=1\n",
            ),
            oversized_path,
        )
    );
    assert_eq!(
        String::from_utf8(skipped.stderr)?,
        "error: the conversation import scan completed with 1 skipped file(s)\n"
    );
    Ok(())
}

/// S28: exact scan replay reports the durable digest match as
/// already imported for that file.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_terminal_client_scan_replays_as_already_imported() -> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let source_directory = tempfile::tempdir()?;
    let source_path = source_directory.path().join("session.jsonl");
    fs::write(
        &source_path,
        concat!(
            "{\"sessionId\":\"terminal-scan-replay\",\"type\":\"user\",",
            "\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"sessionId\":\"terminal-scan-replay\",\"type\":\"assistant\",",
            "\"message\":{\"role\":\"assistant\",\"content\":\"answer\"}}"
        ),
    )?;
    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (eligibility_nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let listener = LocalProcessListener::bind(socket_directory.socket())?;
    let process_runtime = ProcessRuntime::new(
        listener,
        pool.clone(),
        eligibility_nudge,
        InProcessToolDispatchGate::default(),
        support::parse_model_configuration(IMPORT_MODEL_CONFIGURATION)?,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let process_task = tokio::spawn(process_runtime.run(shutdown_receiver));
    let arguments = vec![
        String::from("import"),
        String::from("--format"),
        String::from("claude-code"),
        String::from("--scan"),
        source_directory.path().display().to_string(),
    ];

    let inserted = run_client(
        socket_directory.socket().to_owned(),
        arguments.clone(),
        None,
    )
    .await?;

    assert!(inserted.status.success());
    assert!(inserted.stderr.is_empty());
    let inserted_output = String::from_utf8(inserted.stdout)?;
    let imported_identity = scan_imported_identity(
        inserted_output
            .lines()
            .next()
            .ok_or_else(|| io::Error::other("the scan outcome is absent"))?,
        &source_path,
    )?;
    assert_eq!(
        inserted_output,
        format!(
            "imported path={:?} imported_conversation_id={imported_identity}\n\
             scan_summary imported=1 already_imported=0 skipped=0\n",
            source_path,
        )
    );

    let replayed = run_client(socket_directory.socket().to_owned(), arguments, None).await?;

    assert!(replayed.status.success());
    assert!(replayed.stderr.is_empty());
    assert_eq!(
        String::from_utf8(replayed.stdout)?,
        format!(
            "already_imported path={:?} imported_conversation_id={imported_identity}\n\
             scan_summary imported=0 already_imported=1 skipped=0\n",
            source_path,
        )
    );

    shutdown.send(true)?;
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}

fn scan_imported_identity(line: &str, path: &Path) -> Result<String, Box<dyn Error>> {
    let prefix = format!("imported path={:?} imported_conversation_id=", path);
    let identity = line
        .strip_prefix(&prefix)
        .ok_or_else(|| io::Error::other("the scan outcome did not name the selected path"))?;
    Uuid::parse_str(identity)?;
    Ok(identity.to_owned())
}

/// S33: the terminal model verb observes the complete current
/// defaults facts before sending one recoverable replacement command.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s33_terminal_client_installs_a_forward_only_model_defaults_epoch()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (eligibility_nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let listener = LocalProcessListener::bind(socket_directory.socket())?;
    let process_runtime = ProcessRuntime::new(
        listener,
        pool.clone(),
        eligibility_nudge,
        InProcessToolDispatchGate::default(),
        support::parse_model_configuration(IMPORT_MODEL_CONFIGURATION)?,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let process_task = tokio::spawn(process_runtime.run(shutdown_receiver));
    let first_selection = Uuid::from_u128(1).hyphenated().to_string();
    let second_selection_id = Uuid::from_u128(3);
    let second_selection = second_selection_id.hyphenated().to_string();

    let created = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("create"),
            String::from("--model"),
            first_selection,
        ],
        None,
    )
    .await?;
    assert!(created.status.success());
    let session_id = String::from_utf8(created.stdout)?.trim().to_owned();
    Uuid::parse_str(&session_id)?;

    let replaced = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("model"),
            session_id.clone(),
            String::from("--model"),
            second_selection.clone(),
        ],
        None,
    )
    .await?;
    assert!(replaced.status.success());
    assert_eq!(
        String::from_utf8(replaced.stdout)?,
        format!("session={session_id} defaults_version=2 model={second_selection}\n")
    );
    let recovery = String::from_utf8(replaced.stderr)?;
    assert!(recovery.contains("command_id="));
    assert!(recovery.contains("defaults_version=1\n"));
    assert!(recovery.contains("dangerous_tool_auto_approval=disabled\n"));

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct CurrentDefaultsFacts {
        current_version: i64,
        direct_model_selection_id: Uuid,
        dangerous_tool_auto_approval: String,
    }
    let current: CurrentDefaultsFacts = sqlx::query_as(
        "SELECT current.current_version::bigint,
                defaults.direct_model_selection_id,
                defaults.dangerous_tool_auto_approval
           FROM session_current_defaults AS current
           JOIN session_defaults_version AS defaults
             ON defaults.session_id = current.session_id
            AND defaults.version = current.current_version
          WHERE current.session_id = $1",
    )
    .bind(Uuid::parse_str(&session_id)?)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        current,
        CurrentDefaultsFacts {
            current_version: 2,
            direct_model_selection_id: second_selection_id,
            dangerous_tool_auto_approval: String::from("disabled"),
        }
    );

    shutdown.send(true)?;
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}

struct ImportedContinuationFixture {
    imported_user: String,
    imported_assistant: String,
    live_user: String,
    live_assistant: String,
}

impl ImportedContinuationFixture {
    fn new() -> Self {
        Self {
            imported_user: String::from("synthetic imported question"),
            imported_assistant: String::from("synthetic imported answer"),
            live_user: String::from("synthetic live continuation"),
            live_assistant: String::from("synthetic live reply"),
        }
    }

    fn source(&self) -> String {
        format!(
            "{{\"sessionId\":\"terminal-import-continue\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}\n{{\"sessionId\":\"terminal-import-continue\",\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":\"{}\"}}}}",
            self.imported_user, self.imported_assistant,
        )
    }
}

/// S28 / S01: a synthetic imported prefix seeds a
/// live session whose next real turn follows the imported entries in the
/// authoritative terminal transcript.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_terminal_client_completes_an_offline_imported_continuation()
-> Result<(), Box<dyn Error>> {
    let fixture = ImportedContinuationFixture::new();
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let source_directory = tempfile::tempdir()?;
    let source_path = source_directory.path().join("synthetic-session.jsonl");
    fs::write(&source_path, fixture.source())?;
    let selection_uuid = Uuid::from_u128(0x9201);
    let target_uuid = Uuid::from_u128(0x9202);
    let selection = DirectModelSelection::from_uuid(selection_uuid);
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
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "{selection_uuid}"
target_id = "{target_uuid}"
model_family = "anthropic"
provider_model = "scripted-imported-continuation"
max_output_tokens = 64
context_window_tokens = 200000
"#
    ))?;
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("the fixture target definition is unique");
    let runtime_models =
        RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
            target,
            String::from("scripted-imported-continuation"),
            64,
            200_000,
        )
        .expect("the fixture runtime definition is valid")])
        .expect("the fixture runtime target is unique");
    let runtime = ScriptedModel::single(Script::delivering(TerminalEvidence::Completed(
        CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("scripted-imported-continuation")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(fixture.live_assistant.clone())],
            usage: TokenUsage::unreported(),
        },
    )));
    let provider = RuntimeModelCallProvider::new(runtime, runtime_models, None);

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
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    pool.clone(),
                    targets,
                    ModelCallCredentialReference::new("scripted-imported-continuation"),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            WorkspaceInstructionRuntime::new(pool.clone(), None, Vec::new()),
        ));
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

    let imported = timeout(
        Duration::from_secs(20),
        run_client(
            socket_directory.socket().to_owned(),
            vec![
                String::from("import"),
                String::from("--format"),
                String::from("claude-code"),
                source_path.display().to_string(),
            ],
            None,
        ),
    )
    .await??;
    assert!(imported.status.success());
    let imported_output = String::from_utf8(imported.stdout)?;
    let imported_conversation_id = imported_output
        .strip_prefix("inserted imported_conversation_id=")
        .expect("the synthetic import returns an inserted receipt")
        .trim()
        .to_owned();
    Uuid::parse_str(&imported_conversation_id)?;

    let continued = timeout(
        Duration::from_secs(20),
        run_client(
            socket_directory.socket().to_owned(),
            vec![
                String::from("continue"),
                imported_conversation_id.clone(),
                String::from("--through-position"),
                String::from("2"),
                String::from("--relationship"),
                String::from("resume"),
                String::from("--model"),
                selection.into_uuid().hyphenated().to_string(),
            ],
            None,
        ),
    )
    .await??;
    assert!(continued.status.success());
    let session_id = String::from_utf8(continued.stdout)?.trim().to_owned();
    Uuid::parse_str(&session_id)?;
    assert!(String::from_utf8(continued.stderr)?.starts_with("command_id="));

    let sent = timeout(
        Duration::from_secs(20),
        run_client(
            socket_directory.socket().to_owned(),
            vec![String::from("send"), session_id.clone()],
            Some(fixture.live_user.clone()),
        ),
    )
    .await??;
    assert!(
        sent.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&sent.stderr)
    );
    assert_eq!(
        String::from_utf8(sent.stdout)?,
        format!("{}\n", fixture.live_assistant)
    );
    assert!(!fatal_execution.is_triggered());

    let transcript = run_client(
        socket_directory.socket().to_owned(),
        vec![String::from("transcript"), session_id],
        None,
    )
    .await?;
    assert!(transcript.status.success());
    let transcript = String::from_utf8(transcript.stdout)?;
    let imported_user_label = transcript
        .find("imported_user ")
        .expect("the transcript labels the imported user entry");
    let imported_user_content = transcript
        .find(&fixture.imported_user)
        .expect("the transcript contains the imported user fixture");
    let imported_assistant_label = transcript
        .find("imported_assistant ")
        .expect("the transcript labels the imported assistant entry");
    let imported_assistant_content = transcript
        .find(&fixture.imported_assistant)
        .expect("the transcript contains the imported assistant fixture");
    let live_user_line = transcript
        .lines()
        .find(|line| line.starts_with("user_content source_session="))
        .expect("the transcript labels the live user entry");
    let live_user_label = transcript
        .find(live_user_line)
        .expect("the live user line belongs to the transcript");
    let (identity_fields, parts) = live_user_line
        .strip_prefix("user_content ")
        .and_then(|line| line.split_once(" parts="))
        .expect("the live user entry has canonical metadata and parts");
    let identity_fields = identity_fields.split_whitespace().collect::<Vec<_>>();
    assert_eq!(identity_fields.len(), 4);
    for (field, prefix) in
        identity_fields
            .iter()
            .zip(["source_session=", "entry=", "accepted_input=", "turn="])
    {
        let value = field
            .strip_prefix(prefix)
            .expect("the live user identity fields use canonical labels");
        let parsed =
            uuid::Uuid::parse_str(value).expect("the live user identity fields contain UUIDs");
        assert_eq!(parsed.hyphenated().to_string(), value);
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(parts)?,
        serde_json::json!([{"type": "text", "text": fixture.live_user}])
    );
    let live_user_content = live_user_label
        + live_user_line
            .find(&fixture.live_user)
            .expect("the canonical live user parts contain the fixture");
    let live_assistant_label = transcript
        .find("assistant turn=")
        .expect("the transcript labels the live assistant entry");
    let live_assistant_content = transcript
        .find(&fixture.live_assistant)
        .expect("the transcript contains the live assistant fixture");
    let turn_completed = transcript
        .find("turn_completed ")
        .expect("the transcript contains the live turn terminal marker");
    assert!(imported_user_label < imported_user_content);
    assert!(imported_user_content < imported_assistant_label);
    assert!(imported_assistant_label < imported_assistant_content);
    assert!(imported_assistant_content < live_user_label);
    assert!(live_user_label < live_user_content);
    assert!(live_user_content < live_assistant_label);
    assert!(live_assistant_label < live_assistant_content);
    assert!(live_assistant_content < turn_completed);

    shutdown.send(true)?;
    assert_eq!(
        timeout(Duration::from_secs(10), scheduler_task).await??,
        SchedulerLoopExit::Shutdown
    );
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}

/// S01 / S02: the daily terminal binary drives the real
/// process server, durable outbox, scheduler, model-execution bridge, and
/// authoritative reply reread without network access. A one-step provider
/// proves that hidden physical retry would fail.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn terminal_client_completes_an_offline_scripted_conversation() -> Result<(), Box<dyn Error>>
{
    const FIRST_ASSISTANT_REPLY: &str = "offline assistant reply";
    const SECOND_ASSISTANT_REPLY: &str = "offline assistant reply without usage";
    const REPORTED_INPUT_TOKENS: u64 = 120;
    const REPORTED_OUTPUT_TOKENS: u64 = 7;
    const REPORTED_CACHE_READ_INPUT_TOKENS: u64 = 80;
    const EXPECTED_TERMINAL_CALLS: usize = 2;
    const EXPECTED_LABELED_USAGE_LINES: usize = 4;

    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let selection_uuid = Uuid::from_u128(0x9101);
    let target_uuid = Uuid::from_u128(0x9102);
    let selection = DirectModelSelection::from_uuid(selection_uuid);
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
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "{selection_uuid}"
target_id = "{target_uuid}"
model_family = "anthropic"
provider_model = "scripted-terminal"
max_output_tokens = 64
context_window_tokens = 200000
"#
    ))?;
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("the fixture target definition is unique");
    let runtime_models =
        RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
            target,
            String::from("scripted-terminal"),
            64,
            200_000,
        )
        .expect("the fixture runtime definition is valid")])
        .expect("the fixture runtime target is unique");
    let runtime = ScriptedModel::following([
        Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("scripted-terminal")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(String::from(FIRST_ASSISTANT_REPLY))],
            usage: TokenUsage {
                input_tokens: Some(REPORTED_INPUT_TOKENS),
                output_tokens: Some(REPORTED_OUTPUT_TOKENS),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(REPORTED_CACHE_READ_INPUT_TOKENS),
            },
        })),
        Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("scripted-terminal")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(String::from(SECOND_ASSISTANT_REPLY))],
            usage: TokenUsage::unreported(),
        })),
    ]);
    let provider = RuntimeModelCallProvider::new(runtime, runtime_models, None);

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
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    pool.clone(),
                    targets,
                    ModelCallCredentialReference::new("scripted-terminal"),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            WorkspaceInstructionRuntime::new(pool.clone(), None, Vec::new()),
        ));
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

    let create = timeout(
        Duration::from_secs(20),
        run_client(
            socket_directory.socket().to_owned(),
            vec![
                String::from("create"),
                String::from("--model"),
                selection.into_uuid().hyphenated().to_string(),
            ],
            None,
        ),
    )
    .await??;
    assert!(create.status.success());
    let session_id = String::from_utf8(create.stdout)?.trim().to_owned();
    Uuid::parse_str(&session_id)?;
    assert!(String::from_utf8(create.stderr)?.starts_with("command_id="));

    let list = run_client(
        socket_directory.socket().to_owned(),
        vec![String::from("list")],
        None,
    )
    .await?;
    assert!(list.status.success());
    let listed = String::from_utf8(list.stdout)?;
    assert!(listed.contains(&session_id));
    assert!(listed.contains("defaults_version=1"));

    let send = timeout(
        Duration::from_secs(20),
        run_client(
            socket_directory.socket().to_owned(),
            vec![String::from("send"), session_id.clone()],
            Some(String::from("offline user request")),
        ),
    )
    .await??;
    assert!(
        send.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&send.stderr)
    );
    assert_eq!(
        String::from_utf8(send.stdout)?,
        format!("{FIRST_ASSISTANT_REPLY}\n")
    );
    let recovery = String::from_utf8(send.stderr)?;
    assert!(recovery.contains("command_id="));
    assert!(recovery.contains("defaults_version=1"));
    assert!(!fatal_execution.is_triggered());

    let second_send = timeout(
        Duration::from_secs(20),
        run_client(
            socket_directory.socket().to_owned(),
            vec![String::from("send"), session_id.clone()],
            Some(String::from("offline follow-up request")),
        ),
    )
    .await??;
    assert!(
        second_send.status.success(),
        "second send failed: {}",
        String::from_utf8_lossy(&second_send.stderr)
    );
    assert_eq!(
        String::from_utf8(second_send.stdout)?,
        format!("{SECOND_ASSISTANT_REPLY}\n")
    );
    assert!(!fatal_execution.is_triggered());

    let transcript = run_client(
        socket_directory.socket().to_owned(),
        vec![String::from("transcript"), session_id],
        None,
    )
    .await?;
    assert!(transcript.status.success());
    let transcript = String::from_utf8(transcript.stdout)?;
    assert!(transcript.contains("offline user request"));
    assert!(transcript.contains(FIRST_ASSISTANT_REPLY));
    assert!(transcript.contains(SECOND_ASSISTANT_REPLY));
    assert!(transcript.contains("turn_completed"));
    assert_eq!(
        transcript.matches("usage turn=").count(),
        EXPECTED_LABELED_USAGE_LINES
    );
    assert!(transcript.contains(&format!(
        "terminal_calls=1 input_tokens={REPORTED_INPUT_TOKENS} \
         input_tokens_present_calls=1/1 output_tokens={REPORTED_OUTPUT_TOKENS} \
         output_tokens_present_calls=1/1 cache_creation_input_tokens=unreported \
         cache_creation_input_tokens_present_calls=0/1 \
         cache_read_input_tokens={REPORTED_CACHE_READ_INPUT_TOKENS} \
         cache_read_input_tokens_present_calls=1/1"
    )));
    assert!(transcript.contains(
        "terminal_calls=1 input_tokens=unreported input_tokens_present_calls=0/1 \
         output_tokens=unreported output_tokens_present_calls=0/1 \
         cache_creation_input_tokens=unreported \
         cache_creation_input_tokens_present_calls=0/1 \
         cache_read_input_tokens=unreported cache_read_input_tokens_present_calls=0/1"
    ));
    assert!(transcript.contains(&format!(
        "usage_total scope=session usage_provenance=reported \
         terminal_calls={EXPECTED_TERMINAL_CALLS} \
         input_tokens={REPORTED_INPUT_TOKENS} input_tokens_present_calls=1/2 \
         output_tokens={REPORTED_OUTPUT_TOKENS} output_tokens_present_calls=1/2 \
         cache_creation_input_tokens=unreported \
         cache_creation_input_tokens_present_calls=0/2 \
         cache_read_input_tokens={REPORTED_CACHE_READ_INPUT_TOKENS} \
         cache_read_input_tokens_present_calls=1/2"
    )));

    shutdown.send(true)?;
    assert_eq!(
        timeout(Duration::from_secs(10), scheduler_task).await??,
        SchedulerLoopExit::Shutdown
    );
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}
/// S29: the terminal and real daemon drive an external target through one
/// session-backed read-only pass, atomically bind a finding, and read it back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn terminal_client_drives_review_target_to_finding() -> Result<(), Box<dyn Error>> {
    const SCRIPTED_REVIEW_ASSISTANT_TEXT: &str = "review analysis complete";
    const IS_REAL_CONFIDENCE: &str = "9000";
    const SEVERITY_LABEL_CONFIDENCE: &str = "8500";

    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let selection_uuid = Uuid::from_u128(0x9201);
    let model_target_uuid = Uuid::from_u128(0x9202);
    let selection = DirectModelSelection::from_uuid(selection_uuid);
    let model_target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(model_target_uuid));
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
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "{selection_uuid}"
target_id = "{model_target_uuid}"
model_family = "anthropic"
provider_model = "scripted-review"
max_output_tokens = 64
context_window_tokens = 200000
"#
    ))?;
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        model_target,
    )])
    .expect("the fixture target definition is unique");
    let runtime_models =
        RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
            model_target,
            String::from("scripted-review"),
            64,
            200_000,
        )
        .expect("the fixture runtime definition is valid")])
        .expect("the fixture runtime target is unique");
    let runtime = ScriptedModel::single(Script::delivering(TerminalEvidence::Completed(
        CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("scripted-review")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(String::from(
                SCRIPTED_REVIEW_ASSISTANT_TEXT,
            ))],
            usage: TokenUsage::unreported(),
        },
    )));
    let provider = RuntimeModelCallProvider::new(runtime, runtime_models, None);

    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (eligibility_nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
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

    let create = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("create"),
            String::from("--model"),
            selection_uuid.hyphenated().to_string(),
        ],
        None,
    )
    .await?;
    assert!(create.status.success());
    let session_text = String::from_utf8(create.stdout)?.trim().to_owned();
    let session_uuid = Uuid::parse_str(&session_text)?;

    let send_child = spawn_client(
        socket_directory.socket().to_owned(),
        vec![String::from("send"), session_text.clone()],
        Some(String::from("inspect the frozen change request")),
    )
    .await?;
    let (accepted_input_uuid, turn_uuid) = wait_for_turn_identities(&pool, session_uuid).await?;
    let mut activation = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let activated = require_activated_turn(
        activation
            .execute(SessionId::from_uuid(session_uuid))
            .await?,
    )?;

    let target_uuid = Uuid::from_u128(0x9210);
    let run_uuid = Uuid::from_u128(0x9211);
    let pass_uuid = Uuid::from_u128(0x9212);
    let finding_uuid = Uuid::from_u128(0x9213);
    let activation_command_uuid = Uuid::from_u128(0x9214);
    let finding_command_uuid = Uuid::from_u128(0x9215);
    let target_text = target_uuid.hyphenated().to_string();
    let activation_recovery_command_uuid = Uuid::from_u128(0x9216);
    let finding_recovery_command_uuid = Uuid::from_u128(0x9217);
    let run_text = run_uuid.hyphenated().to_string();
    let pass_text = pass_uuid.hyphenated().to_string();
    let finding_text = finding_uuid.hyphenated().to_string();

    let target_created = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("review"),
            String::from("create-target"),
            target_text.clone(),
            String::from("--provider"),
            String::from("example-host"),
            String::from("--repository"),
            String::from("owner/repository"),
            String::from("--change-request"),
            String::from("42"),
            String::from("--head-revision"),
            String::from("head-revision"),
            String::from("--base-revision"),
            String::from("base-revision"),
        ],
        None,
    )
    .await?;
    assert!(
        target_created.status.success(),
        "target creation failed: {}",
        String::from_utf8_lossy(&target_created.stderr),
    );

    let run_started = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("review"),
            String::from("start-run"),
            target_text.clone(),
            run_text.clone(),
            pass_text.clone(),
            String::from("--workflow"),
            String::from("read-only-review"),
            String::from("--session-id"),
            session_text.clone(),
            String::from("--accepted-input-id"),
            accepted_input_uuid.hyphenated().to_string(),
        ],
        None,
    )
    .await?;
    assert!(
        run_started.status.success(),
        "run creation failed: {}",
        String::from_utf8_lossy(&run_started.stderr),
    );

    let activate_arguments = vec![
        String::from("review"),
        String::from("activate-pass"),
        run_text.clone(),
        pass_text.clone(),
        String::from("--turn-id"),
        turn_uuid.hyphenated().to_string(),
        String::from("--command-id"),
        activation_command_uuid.hyphenated().to_string(),
    ];
    let pass_activated = run_client(
        socket_directory.socket().to_owned(),
        activate_arguments.clone(),
        None,
    )
    .await?;
    assert!(
        pass_activated.status.success(),
        "pass activation failed: {}",
        String::from_utf8_lossy(&pass_activated.stderr),
    );
    let activation_replay = run_client(
        socket_directory.socket().to_owned(),
        activate_arguments,
        None,
    )
    .await?;
    assert_eq!(activation_replay.status, pass_activated.status);
    assert_eq!(activation_replay.stdout, pass_activated.stdout);
    assert_eq!(activation_replay.stderr, pass_activated.stderr);
    let activation_recovery = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("review"),
            String::from("activate-pass"),
            run_text.clone(),
            pass_text.clone(),
            String::from("--turn-id"),
            turn_uuid.hyphenated().to_string(),
            String::from("--command-id"),
            activation_recovery_command_uuid.hyphenated().to_string(),
        ],
        None,
    )
    .await?;
    assert_eq!(activation_recovery.status, pass_activated.status);
    assert_eq!(activation_recovery.stdout, pass_activated.stdout);
    assert_eq!(activation_recovery.stderr, pass_activated.stderr);

    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    pool.clone(),
                    targets,
                    ModelCallCredentialReference::new("scripted-review"),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            WorkspaceInstructionRuntime::new(pool.clone(), None, Vec::new()),
        ));
    execution.execute(activated).await?;
    assert!(!fatal_execution.is_triggered());
    let send = timeout(Duration::from_secs(10), send_child.wait_with_output())
        .await
        .map_err(|_| io::Error::new(ErrorKind::TimedOut, "review send client did not finish"))??;
    assert!(send.status.success());
    assert_eq!(
        String::from_utf8(send.stdout)?,
        format!("{SCRIPTED_REVIEW_ASSISTANT_TEXT}\n")
    );
    let terminal_frontier: Uuid = sqlx::query_scalar(
        "SELECT terminal_frontier_id
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(turn_uuid)
    .fetch_one(&pool)
    .await?;

    let record_arguments = vec![
        String::from("review"),
        String::from("record-finding"),
        run_text.clone(),
        pass_text.clone(),
        String::from("--turn-id"),
        turn_uuid.hyphenated().to_string(),
        String::from("--output-frontier-id"),
        terminal_frontier.hyphenated().to_string(),
        String::from("--finding-id"),
        finding_text.clone(),
        String::from("--file-path"),
        String::from("src/lib.rs"),
        String::from("--line-start"),
        String::from("7"),
        String::from("--line-end"),
        String::from("7"),
        String::from("--diff-side"),
        String::from("right"),
        String::from("--title"),
        String::from("Unsafe terminal text"),
        String::from("--body"),
        String::from("body\u{1b}[31m"),
        String::from("--severity"),
        String::from("high"),
        String::from("--is-real-confidence"),
        String::from(IS_REAL_CONFIDENCE),
        String::from("--severity-label-confidence"),
        String::from(SEVERITY_LABEL_CONFIDENCE),
        String::from("--category"),
        String::from("correctness"),
        String::from("--command-id"),
        finding_command_uuid.hyphenated().to_string(),
    ];
    let recorded = run_client(
        socket_directory.socket().to_owned(),
        record_arguments.clone(),
        None,
    )
    .await?;
    assert!(
        recorded.status.success(),
        "finding admission failed: {}",
        String::from_utf8_lossy(&recorded.stderr),
    );
    let finding_replay =
        run_client(socket_directory.socket().to_owned(), record_arguments, None).await?;
    assert_eq!(finding_replay.status, recorded.status);
    assert_eq!(finding_replay.stdout, recorded.stdout);
    assert_eq!(finding_replay.stderr, recorded.stderr);
    let finding_recovery = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("review"),
            String::from("record-finding"),
            run_text.clone(),
            pass_text.clone(),
            String::from("--turn-id"),
            turn_uuid.hyphenated().to_string(),
            String::from("--output-frontier-id"),
            terminal_frontier.hyphenated().to_string(),
            String::from("--finding-id"),
            finding_text.clone(),
            String::from("--file-path"),
            String::from("src/lib.rs"),
            String::from("--line-start"),
            String::from("7"),
            String::from("--line-end"),
            String::from("7"),
            String::from("--diff-side"),
            String::from("right"),
            String::from("--title"),
            String::from("Unsafe terminal text"),
            String::from("--body"),
            String::from("body\u{1b}[31m"),
            String::from("--severity"),
            String::from("high"),
            String::from("--is-real-confidence"),
            String::from(IS_REAL_CONFIDENCE),
            String::from("--severity-label-confidence"),
            String::from(SEVERITY_LABEL_CONFIDENCE),
            String::from("--category"),
            String::from("correctness"),
            String::from("--command-id"),
            finding_recovery_command_uuid.hyphenated().to_string(),
        ],
        None,
    )
    .await?;
    assert_eq!(finding_recovery.status, recorded.status);
    assert_eq!(finding_recovery.stdout, recorded.stdout);
    assert_eq!(finding_recovery.stderr, recorded.stderr);

    let read_run = run_client(
        socket_directory.socket().to_owned(),
        vec![String::from("review"), String::from("read-run"), run_text],
        None,
    )
    .await?;
    assert!(read_run.status.success());
    assert!(String::from_utf8(read_run.stdout)?.contains("state=succeeded"));
    let read_finding = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("review"),
            String::from("read-finding"),
            finding_text,
        ],
        None,
    )
    .await?;
    assert!(read_finding.status.success());
    let finding_output = String::from_utf8(read_finding.stdout)?;
    assert!(finding_output.contains("status=open"));
    assert!(finding_output.contains(&format!("is_real_confidence={IS_REAL_CONFIDENCE}")));
    assert!(finding_output.contains(&format!(
        "severity_label_confidence={SEVERITY_LABEL_CONFIDENCE}"
    )));
    assert!(finding_output.contains("body=body\\u{1b}[31m"));
    assert!(!finding_output.contains('\u{1b}'));
    let listed = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("review"),
            String::from("list-findings"),
            run_uuid.hyphenated().to_string(),
        ],
        None,
    )
    .await?;
    assert!(listed.status.success());
    assert!(String::from_utf8(listed.stdout)?.contains(&finding_uuid.hyphenated().to_string()));

    shutdown.send(true)?;
    timeout(Duration::from_secs(10), process_task)
        .await
        .map_err(|_| io::Error::new(ErrorKind::TimedOut, "review process did not stop"))???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
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

/// Waits until the spawned send client durably admits its turn identities.
async fn wait_for_turn_identities(
    pool: &PgPool,
    session: Uuid,
) -> Result<(Uuid, Uuid), Box<dyn Error>> {
    let identities = timeout(Duration::from_secs(20), async {
        loop {
            let identities = sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT origin_accepted_input_id, turn_id
                   FROM turn_lifecycle
                  WHERE session_id = $1",
            )
            .bind(session)
            .fetch_optional(pool)
            .await?;
            if let Some(identities) = identities {
                return Ok::<(Uuid, Uuid), sqlx::Error>(identities);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| io::Error::other("the review send never admitted its turn"))??;
    Ok(identities)
}

/// Runs `transcript` until the session's turn parks on its approval wait, then
/// returns the pending request identity printed for the approver.
async fn wait_for_pending_approval(
    socket: &Path,
    session_id: &str,
) -> Result<String, Box<dyn Error>> {
    timeout(Duration::from_secs(20), async {
        loop {
            let transcript = run_client(
                socket.to_owned(),
                vec![String::from("transcript"), session_id.to_owned()],
                None,
            )
            .await?;
            if transcript.status.success() {
                let rendered = String::from_utf8(transcript.stdout)?;
                if let Some(request) = rendered.lines().find_map(|line| {
                    line.split_once("state=active_awaiting_tool_approval request=")
                        .map(|(_, request)| request.trim().to_owned())
                }) {
                    if !rendered.contains("assistant_tool_use") {
                        return Err::<String, Box<dyn Error>>(
                            io::Error::other(
                                "the awaiting transcript must render the proposing tool use",
                            )
                            .into(),
                        );
                    }
                    return Ok(request);
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| io::Error::other("the fixture turn never parked on its approval wait"))?
}

/// S10: while one client's `send` keeps waiting on the approval
/// wait, a second client reads the pending request from the transcript and
/// approves it; the tool executes, the continuation round completes, and the
/// waiting `send` prints the final scripted reply.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn terminal_client_approval_from_a_second_client_completes_a_waiting_send()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let selection_uuid = Uuid::from_u128(0x9301);
    let target_uuid = Uuid::from_u128(0x9302);
    let selection = DirectModelSelection::from_uuid(selection_uuid);
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
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "{selection_uuid}"
target_id = "{target_uuid}"
model_family = "anthropic"
provider_model = "scripted-approval"
max_output_tokens = 64
context_window_tokens = 200000
"#
    ))?;
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("the fixture target definition is unique");
    let runtime_models =
        RuntimeModelCatalog::try_from_definitions([RuntimeModelDefinition::try_new(
            target,
            String::from("scripted-approval"),
            64,
            200_000,
        )
        .expect("the fixture runtime definition is valid")])
        .expect("the fixture runtime target is unique");
    let runtime = ScriptedModel::following([
        Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("scripted-approval")),
            finish: CompletionFinish::ToolUse,
            content: vec![AssistantPart::ToolCall(RuntimeToolCallProposal {
                id: ToolCallId::new(String::from("fixture-call-0")),
                name: RuntimeToolName::new("confirmed_probe"),
                arguments_json: String::from("{}"),
            })],
            usage: TokenUsage::unreported(),
        })),
        Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("scripted-approval")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(String::from("approved tool reply"))],
            usage: TokenUsage::unreported(),
        })),
    ]);
    let provider = RuntimeModelCallProvider::new(runtime, runtime_models, None);
    let tool_catalog = CompiledToolCatalog::try_new([CompiledTool::new(
        ToolDefinition::new(
            ToolName::try_new(String::from("confirmed_probe"))
                .expect("the fixture tool name is valid"),
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
    let (execution, fatal_execution) = FatalExecutionSupervisor::new(
        PostgresProviderModelExecution::new(
            PostgresModelCallRepository::new(
                pool.clone(),
                targets,
                ModelCallCredentialReference::new("scripted-approval"),
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

    let create = timeout(
        Duration::from_secs(20),
        run_client(
            socket_directory.socket().to_owned(),
            vec![
                String::from("create"),
                String::from("--model"),
                selection.into_uuid().hyphenated().to_string(),
            ],
            None,
        ),
    )
    .await??;
    assert!(create.status.success());
    let session_id = String::from_utf8(create.stdout)?.trim().to_owned();

    let send_socket = socket_directory.socket().to_owned();
    let send_session = session_id.clone();
    let waiting_send = tokio::spawn(async move {
        run_client(
            send_socket,
            vec![String::from("send"), send_session],
            Some(String::from("use the confirmed probe")),
        )
        .await
        .map_err(|error| io::Error::other(error.to_string()))
    });

    let pending_request = wait_for_pending_approval(socket_directory.socket(), &session_id).await?;
    Uuid::parse_str(&pending_request)?;

    let approve = timeout(
        Duration::from_secs(20),
        run_client(
            socket_directory.socket().to_owned(),
            vec![
                String::from("approve"),
                session_id.clone(),
                pending_request.clone(),
            ],
            None,
        ),
    )
    .await??;
    assert!(
        approve.status.success(),
        "approve failed: {}",
        String::from_utf8_lossy(&approve.stderr)
    );
    assert_eq!(
        String::from_utf8(approve.stdout)?,
        format!("tool_request={pending_request} decision=approve\n")
    );
    assert!(String::from_utf8(approve.stderr)?.starts_with("command_id="));

    let send = timeout(Duration::from_secs(30), waiting_send).await???;
    assert!(
        send.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&send.stderr)
    );
    assert_eq!(String::from_utf8(send.stdout)?, "approved tool reply\n");
    assert!(!fatal_execution.is_triggered());

    let transcript = run_client(
        socket_directory.socket().to_owned(),
        vec![String::from("transcript"), session_id],
        None,
    )
    .await?;
    assert!(transcript.status.success());
    let transcript = String::from_utf8(transcript.stdout)?;
    assert!(transcript.contains("name=confirmed_probe"));
    assert!(transcript.contains("content=completed:confirmed_probe"));
    assert!(transcript.contains("approved tool reply"));
    assert!(transcript.contains("turn_completed"));

    shutdown.send(true)?;
    assert_eq!(
        timeout(Duration::from_secs(10), scheduler_task).await??,
        SchedulerLoopExit::Shutdown
    );
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}

/// S02: an explicitly opted-in smoke test drives the same
/// terminal and process boundary through the production Anthropic runtime
/// adapter. It requires a reviewed model catalog, a credential file, and a
/// direct selection identity supplied by the operator.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PostgreSQL, a local socket, and an explicitly configured real Anthropic call"]
async fn terminal_client_completes_the_real_anthropic_path() -> Result<(), Box<dyn Error>> {
    let configuration_file = PathBuf::from(required_environment("SIGNALBOX_E2E_CONFIG_FILE")?);
    let selection_uuid = required_canonical_uuid_environment("SIGNALBOX_E2E_SELECTION_ID")?;

    let model_configuration = HubModelConfiguration::read(&configuration_file)?;
    let selection = DirectModelSelection::from_uuid(selection_uuid);
    let credential_profile = model_configuration
        .resolve_direct_model(selection)
        .filter(|route| route.uses_anthropic_adapter())
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "SIGNALBOX_E2E_SELECTION_ID must select the Anthropic adapter",
            )
        })?
        .credential_profile()
        .to_owned();
    let credential_access = FileCredentialAccess::from_files(
        model_configuration
            .file_credential_profiles(ModelAdapter::Anthropic)
            .map(|(reference, path)| (CredentialReference::new(reference), path.to_path_buf())),
    );
    let credential_reference = ModelCallCredentialReference::new(credential_profile);
    let anthropic = AnthropicRuntime::new(AnthropicConfig::new(None), credential_access)?;
    let provider =
        RuntimeModelCallProvider::new(anthropic, model_configuration.runtime_model_catalog(), None);
    let targets = model_configuration.target_catalog();
    // Captured before the configuration moves into the process runtime.
    // Production composition attaches this catalog so every call resolves the
    // credential its session pinned for the *serving* family; under fast mode
    // that is the alternate target's family, not the selected one. Without it
    // the repository falls back to one reference for every call and the smoke
    // would authenticate a fast call with the base family's account.
    let credential_families = model_configuration.credential_family_catalog();

    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
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
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(pool.clone(), targets, credential_reference)
                    .with_session_credentials(credential_families),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            WorkspaceInstructionRuntime::new(pool.clone(), None, Vec::new()),
        ));
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

    let create = timeout(
        Duration::from_secs(20),
        run_client(
            socket_directory.socket().to_owned(),
            vec![
                String::from("create"),
                String::from("--model"),
                selection_uuid.hyphenated().to_string(),
            ],
            None,
        ),
    )
    .await??;
    assert!(
        create.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&create.stderr)
    );
    let session_id = String::from_utf8(create.stdout)?.trim().to_owned();

    let send = timeout(
        Duration::from_secs(180),
        run_client(
            socket_directory.socket().to_owned(),
            vec![String::from("send"), session_id],
            Some(String::from(
                "Reply with exactly: signalbox terminal smoke ok",
            )),
        ),
    )
    .await??;
    assert!(
        send.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&send.stderr)
    );
    assert_eq!(
        String::from_utf8(send.stdout)?.trim(),
        "signalbox terminal smoke ok"
    );
    assert!(!fatal_execution.is_triggered());

    shutdown.send(true)?;
    assert_eq!(
        timeout(Duration::from_secs(10), scheduler_task).await??,
        SchedulerLoopExit::Shutdown
    );
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}

/// S34: the terminal client creates a prompted
/// session from a file, copies the exact prompt forward through a model-only
/// replacement, and clears it explicitly, with the immutable epoch rows
/// holding the exact text throughout.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s34_terminal_client_carries_the_session_system_prompt() -> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (eligibility_nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let listener = LocalProcessListener::bind(socket_directory.socket())?;
    let process_runtime = ProcessRuntime::new(
        listener,
        pool.clone(),
        eligibility_nudge,
        InProcessToolDispatchGate::default(),
        support::parse_model_configuration(IMPORT_MODEL_CONFIGURATION)?,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let process_task = tokio::spawn(process_runtime.run(shutdown_receiver));
    let first_selection = Uuid::from_u128(1).hyphenated().to_string();
    let second_selection = Uuid::from_u128(3).hyphenated().to_string();
    let prompt_text = "exact review instructions\nsecond exact line\n";
    let prompt_directory = tempfile::tempdir()?;
    let prompt_path = prompt_directory.path().join("system-prompt.txt");
    std::fs::write(&prompt_path, prompt_text)?;

    let created = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("create"),
            String::from("--model"),
            first_selection.clone(),
            String::from("--system-prompt-file"),
            prompt_path.display().to_string(),
        ],
        None,
    )
    .await?;
    assert!(created.status.success());
    let session_id = String::from_utf8(created.stdout)?.trim().to_owned();
    let session_uuid = Uuid::parse_str(&session_id)?;

    let stored_initial: Option<String> = sqlx::query_scalar(
        "SELECT system_prompt FROM session_defaults_version
          WHERE session_id = $1 AND version = 1",
    )
    .bind(session_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_initial.as_deref(), Some(prompt_text));

    // A model-only replacement copies the exact prompt forward.
    let replaced = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("model"),
            session_id.clone(),
            String::from("--model"),
            second_selection.clone(),
        ],
        None,
    )
    .await?;
    assert!(replaced.status.success());
    assert_eq!(
        String::from_utf8(replaced.stdout)?,
        format!("session={session_id} defaults_version=2 model={second_selection}\n")
    );
    let recovery = String::from_utf8(replaced.stderr)?;
    assert!(recovery.contains("defaults_version=1\n"));
    assert!(recovery.contains("dangerous_tool_auto_approval=disabled\n"));
    assert!(!recovery.contains("exact review instructions"));
    let stored_copied: Option<String> = sqlx::query_scalar(
        "SELECT system_prompt FROM session_defaults_version
          WHERE session_id = $1 AND version = 2",
    )
    .bind(session_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_copied.as_deref(), Some(prompt_text));

    // An explicit clear installs a promptless successor while the prompted
    // epochs remain exact history.
    let cleared = run_client(
        socket_directory.socket().to_owned(),
        vec![
            String::from("model"),
            session_id.clone(),
            String::from("--model"),
            first_selection.clone(),
            String::from("--clear-system-prompt"),
        ],
        None,
    )
    .await?;
    assert!(cleared.status.success());
    let stored_cleared: Option<String> = sqlx::query_scalar(
        "SELECT system_prompt FROM session_defaults_version
          WHERE session_id = $1 AND version = 3",
    )
    .bind(session_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_cleared, None);
    let stored_history: Option<String> = sqlx::query_scalar(
        "SELECT system_prompt FROM session_defaults_version
          WHERE session_id = $1 AND version = 1",
    )
    .bind(session_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_history.as_deref(), Some(prompt_text));

    shutdown.send(true)?;
    timeout(Duration::from_secs(10), process_task).await???;
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}
