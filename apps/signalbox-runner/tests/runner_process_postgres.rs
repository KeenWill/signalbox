#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::{error::Error, fs, os::unix::fs::PermissionsExt as _, process::Stdio, time::Duration};

use signalbox_persistence::{
    disposable_test_container_labels, local_test_connection_options, migrate,
};
use signalbox_test_bin::test_bin_path;
use signalboxd::{
    LocalProcessListener,
    runner_protocol_runtime::{PostgresRunnerRegistrationService, RunnerProtocolRuntime},
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
    sync::watch,
    time::timeout,
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_runner_process";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);

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
        runner_binary.display(),
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
