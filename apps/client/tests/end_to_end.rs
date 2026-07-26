#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::{
    env,
    error::Error,
    ffi::OsString,
    fs,
    io::{self, ErrorKind},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

use signalbox_application::{
    InProcessAttemptDispatchGate, InProcessEligibilityWorkSource, InProcessToolDispatchGate,
    ModelCallCredentialReference, SchedulerLoop, SchedulerLoopExit, StartEligibleTurnService,
    UuidV7StartEligibleTurnIdGenerator,
};
use signalbox_domain::{
    DirectModelSelection, ModelTargetCatalog, ModelTargetDefinition, ProviderModelIdentity,
    ResolvedProviderTarget,
};
use signalbox_hubd::{
    ANTHROPIC_CREDENTIAL_REFERENCE, ActivatedTurnPass, FatalExecutionSupervisor,
    FileCredentialAccess, HubModelConfiguration, LocalProcessListener,
    PostgresProviderModelExecution, ProcessRuntime,
};
use signalbox_model_provider_runtime::{
    RuntimeModelCallProvider, RuntimeModelCatalog, RuntimeModelDefinition,
};
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, CredentialReference, ExchangeFacts,
    ProviderReportedModel, Script, ScriptedModel, TerminalEvidence, TokenUsage,
};
use signalbox_model_runtime_anthropic::{AnthropicConfig, AnthropicRuntime};
use signalbox_persistence::{
    local_test_connection_options, migrate, model_execution::PostgresModelCallRepository,
    scheduler::PostgresEligibilitySweep, start_eligible_turn::StartEligibleTurnRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{io::AsyncWriteExt, process::Command, sync::watch, time::timeout};
use uuid::Uuid;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_terminal_client";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const IMPORT_MODEL_CONFIGURATION: &str = r#"
version = 1

[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000002"
provider = "anthropic"
provider_model = "import-fixture"
max_output_tokens = 64

[[models]]
selection_id = "00000000-0000-0000-0000-000000000003"
target_id = "00000000-0000-0000-0000-000000000004"
provider = "anthropic"
provider_model = "import-fixture-next"
max_output_tokens = 64
"#;

async fn postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_signalbox"));
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
    Ok(child.wait_with_output().await?)
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

/// S28 / INV-038: the shipped terminal verb reads one named file and exposes
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
        HubModelConfiguration::parse(IMPORT_MODEL_CONFIGURATION)?,
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

/// S30 / INV-008 / INV-040: the terminal model verb observes the complete current
/// defaults facts before sending one recoverable replacement command.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s30_inv008_inv040_terminal_client_installs_a_forward_only_model_defaults_epoch()
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
        HubModelConfiguration::parse(IMPORT_MODEL_CONFIGURATION)?,
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

/// S01 / S02 / INV-014 / INV-032: the daily terminal binary drives the real
/// process server, durable outbox, scheduler, model-execution bridge, and
/// authoritative reply reread without network access. A one-step provider
/// proves that hidden physical retry would fail.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn terminal_client_completes_an_offline_scripted_conversation() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let selection_uuid = Uuid::from_u128(0x9101);
    let target_uuid = Uuid::from_u128(0x9102);
    let selection = DirectModelSelection::from_uuid(selection_uuid);
    let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(target_uuid));
    let model_configuration = HubModelConfiguration::parse(&format!(
        r#"
version = 1

[[models]]
selection_id = "{selection_uuid}"
target_id = "{target_uuid}"
provider = "anthropic"
provider_model = "scripted-terminal"
max_output_tokens = 64
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
        )
        .expect("the fixture runtime definition is valid")])
        .expect("the fixture runtime target is unique");
    let runtime = ScriptedModel::single(Script::delivering(TerminalEvidence::Completed(
        CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("scripted-terminal")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(String::from("offline assistant reply"))],
            usage: TokenUsage::unreported(),
        },
    )));
    let provider = RuntimeModelCallProvider::new(runtime, runtime_models);

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
        FatalExecutionSupervisor::new(PostgresProviderModelExecution::new(
            PostgresModelCallRepository::new(
                pool.clone(),
                targets,
                ModelCallCredentialReference::new("scripted-terminal"),
            ),
            InProcessAttemptDispatchGate::default(),
            provider,
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
    assert_eq!(String::from_utf8(send.stdout)?, "offline assistant reply\n");
    let recovery = String::from_utf8(send.stderr)?;
    assert!(recovery.contains("command_id="));
    assert!(recovery.contains("defaults_version=1"));
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
    assert!(transcript.contains("offline assistant reply"));
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

/// S02 / INV-014 / INV-032: an explicitly opted-in smoke test drives the same
/// terminal and process boundary through the production Anthropic runtime
/// adapter. It requires a reviewed model catalog, a credential file, and a
/// direct selection identity supplied by the operator.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PostgreSQL, a local socket, and an explicitly configured real Anthropic call"]
async fn terminal_client_completes_the_real_anthropic_path() -> Result<(), Box<dyn Error>> {
    let configuration_file = PathBuf::from(required_environment("SIGNALBOX_E2E_CONFIG_FILE")?);
    let credential_file = PathBuf::from(required_environment(
        "SIGNALBOX_E2E_ANTHROPIC_API_KEY_FILE",
    )?);
    let selection_text = required_environment("SIGNALBOX_E2E_SELECTION_ID")?
        .into_string()
        .map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "SIGNALBOX_E2E_SELECTION_ID must be valid UTF-8",
            )
        })?;
    let selection_uuid = Uuid::parse_str(&selection_text)?;
    if selection_uuid.hyphenated().to_string() != selection_text {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "SIGNALBOX_E2E_SELECTION_ID must be canonical lowercase UUID text",
        )
        .into());
    }

    let model_configuration = HubModelConfiguration::read(&configuration_file)?;
    let credential_access = FileCredentialAccess::new(
        credential_file,
        CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
    );
    let credential_reference =
        ModelCallCredentialReference::new(credential_access.credential_reference().as_str());
    let anthropic = AnthropicRuntime::new(AnthropicConfig::new(), credential_access)?;
    let provider =
        RuntimeModelCallProvider::new(anthropic, model_configuration.runtime_model_catalog());
    let targets = model_configuration.target_catalog();

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
        FatalExecutionSupervisor::new(PostgresProviderModelExecution::new(
            PostgresModelCallRepository::new(pool.clone(), targets, credential_reference),
            InProcessAttemptDispatchGate::default(),
            provider,
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
                selection_text,
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
