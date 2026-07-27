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
    os::unix::fs::{PermissionsExt, symlink},
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
use signalbox_process_protocol::{
    CanonicalUuid, ClientFrame, ClientRequest, CommandId, ProtocolVersion, RequestId,
    ServerMessage, SessionMetadata, decode_server_line, encode_client_line,
};
use signalboxd::{
    ANTHROPIC_CREDENTIAL_REFERENCE, ActivatedTurnPass, FatalExecutionSupervisor,
    FileCredentialAccess, HubModelConfiguration, LocalProcessListener,
    PostgresProviderModelExecution, ProcessRuntime, ProcessRuntimeError,
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
/// A synthetic source larger than any complete import request can admit.
const OVERSIZED_IMPORT_BYTES: u64 = 8 * 1024 * 1024;
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

/// The direct model selection every metadata-search fixture session carries:
/// the first selection `IMPORT_MODEL_CONFIGURATION` defines, since the search
/// verb reads metadata and never depends on which model a session selected.
const SEARCH_FIXTURE_SELECTION: &str = "00000000-0000-0000-0000-000000000001";

/// The process server the metadata-search tests drive. They start no turn, so
/// the fixture runs the process boundary without a scheduler or provider.
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
            HubModelConfiguration::parse(IMPORT_MODEL_CONFIGURATION)?,
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

/// Installs one complete metadata snapshot through the version-four process
/// request, which no terminal verb exposes.
async fn replace_fixture_metadata(
    socket: &Path,
    session_id: &str,
    metadata: SessionMetadata,
) -> Result<(), Box<dyn Error>> {
    let stream = UnixStream::connect(socket).await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::Four,
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
         last_writer=owner updated_at_unix_micros="
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

/// S28 / INV-038: scan mode selects recursive matching regular files and
/// reports them in deterministic path order.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_inv038_terminal_client_scan_selects_recursive_files_in_sorted_path_order()
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
        HubModelConfiguration::parse(IMPORT_MODEL_CONFIGURATION)?,
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

/// S28 / INV-038: scan mode reports an oversized selected source as skipped
/// without truncating it or hiding the failure total.
#[tokio::test]
async fn s28_inv038_terminal_client_scan_skips_oversized_source_without_truncation()
-> Result<(), Box<dyn Error>> {
    let source_directory = tempfile::tempdir()?;
    let oversized_path = source_directory.path().join("oversized.jsonl");
    std::fs::File::create(&oversized_path)?.set_len(OVERSIZED_IMPORT_BYTES)?;
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
            "skipped path={:?} reason=the conversation import source cannot fit within the process \
             frame bound\nscan_summary imported=0 already_imported=0 skipped=1\n",
            oversized_path,
        )
    );
    assert_eq!(
        String::from_utf8(skipped.stderr)?,
        "error: the conversation import scan completed with 1 skipped file(s)\n"
    );
    Ok(())
}

/// S28 / INV-038: exact scan replay reports the durable digest match as
/// already imported for that file.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_inv038_terminal_client_scan_replays_as_already_imported() -> Result<(), Box<dyn Error>>
{
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
        HubModelConfiguration::parse(IMPORT_MODEL_CONFIGURATION)?,
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

/// S33 / INV-008 / INV-046: the terminal model verb observes the complete current
/// defaults facts before sending one recoverable replacement command.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s33_inv008_inv046_terminal_client_installs_a_forward_only_model_defaults_epoch()
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
