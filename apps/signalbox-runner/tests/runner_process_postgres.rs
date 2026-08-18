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

use signalbox_domain::{
    CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
    RunnerSandboxProfile, RunnerSelector, RunnerToolPermissionOverrides, RunnerWorkingDirectory,
    SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
    SessionRunnerPlacementRequest, SessionRunnerPlacementState, TranscriptAncestry,
    WorkingDirectorySelection, WorkspaceRequirement,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    disposable_test_container_labels, local_test_connection_options, migrate,
    runner_protocol::{
        RunnerConnectionCause, RunnerConnectionState, RunnerConnectionTransition,
        RunnerConnectionTransitionOutcome,
    },
    session_credentials::{SessionCredentialPin, SessionModelCredential},
};
use signalbox_runner_wire::{
    Advertise, CanonicalUuid, Enroll, PositiveU64, Registered, Resume, Resumed,
};
use signalbox_test_bin::test_bin_path;
use signalboxd::{
    LocalProcessListener,
    runner_protocol_runtime::{
        PostgresRunnerRegistrationService, RunnerEnrollmentAccepted, RunnerProtocolRuntime,
        RunnerRegistrationFuture, RunnerRegistrationService,
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

#[derive(Clone)]
struct LossObservedRegistrationService {
    inner: PostgresRunnerRegistrationService,
    completed: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl RunnerRegistrationService for LossObservedRegistrationService {
    fn enroll(&self, request: Enroll) -> RunnerRegistrationFuture<'_, RunnerEnrollmentAccepted> {
        self.inner.enroll(request)
    }

    fn resume(&self, request: Resume) -> RunnerRegistrationFuture<'_, Resumed> {
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
        let completed = Arc::clone(&self.completed);
        Box::pin(async move {
            let outcome = inner
                .transition_connection(enrollment, epoch, transition)
                .await?;
            completed
                .lock()
                .expect("the loss-observation sender lock remains available")
                .take()
                .expect("the runtime reports one terminal connection transition")
                .send(())
                .expect("the loss observer remains live");
            Ok(outcome)
        })
    }
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
    let runner_binary = test_bin_path!("signalbox-runner");
    let configuration = format!(
        r#"version = 1
daemon_socket_path = "{}"
runner_root = "{}"
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
    let (loss_sender, loss_observer) = oneshot::channel();
    let service = LossObservedRegistrationService {
        inner,
        completed: Arc::new(Mutex::new(Some(loss_sender))),
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
    timeout(PROCESS_TIMEOUT, loss_observer)
        .await
        .expect("the daemon propagates runner loss before the integration deadline")?;
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
