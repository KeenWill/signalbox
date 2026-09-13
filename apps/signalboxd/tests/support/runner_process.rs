use std::{
    error::Error,
    fs, io,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

use signalbox_persistence::{
    disposable_postgres_server_args, disposable_test_container_labels,
    local_test_connection_options,
};
use signalbox_test_bin::{resolve, test_bin_path};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tempfile::TempDir;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt as _, core::ExecCommand, runners::AsyncRunner as _},
};
use tokio::{
    process::{Child, Command},
    time::{sleep, timeout},
};

#[path = "../../../../tooling/postgres_test_image.rs"]
mod postgres_test_image;

pub const PROCESS_ALLOWANCE: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct RunnerProcesses {
    pub pool: PgPool,
    pub runner_root: PathBuf,
    runner_config: PathBuf,
    daemon: Child,
    directory: TempDir,
    _postgres: ContainerAsync<Postgres>,
}

impl RunnerProcesses {
    pub async fn start() -> Result<Self, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        let certificate = root.join("postgres.crt");
        let key = root.join("postgres.key");
        let generated = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=runner-test",
                "-addext",
                "subjectAltName=IP:127.0.0.1",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-addext",
                "extendedKeyUsage=serverAuth",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&certificate)
            .output()
            .await?;
        assert!(
            generated.status.success(),
            "fixture certificate generation failed: {:?}",
            generated
        );
        let postgres = Postgres::default()
            .with_user("signalbox")
            .with_password("signalbox-test-only")
            .with_db_name("runner_test")
            .with_tag(postgres_test_image::POSTGRES_IMAGE_TAG)
            .with_cmd(disposable_postgres_server_args())
            .with_labels(disposable_test_container_labels())
            .with_copy_to("/tmp/runner-test.crt", certificate.clone())
            .with_copy_to("/tmp/runner-test.key", key)
            .start()
            .await?;
        let mut configured = postgres.exec(ExecCommand::new([
            "sh", "-ec",
            "chown postgres:postgres /tmp/runner-test.key; chmod 600 /tmp/runner-test.key; psql -U signalbox -d runner_test -c \"ALTER SYSTEM SET ssl_cert_file = '/tmp/runner-test.crt'\"; psql -U signalbox -d runner_test -c \"ALTER SYSTEM SET ssl_key_file = '/tmp/runner-test.key'\"; psql -U signalbox -d runner_test -c \"ALTER SYSTEM SET ssl = 'on'\"; psql -U signalbox -d runner_test -c 'SELECT pg_reload_conf()'",
        ])).await?;
        let configuration_output = configured.stdout_to_vec().await?;
        assert_eq!(
            configured.exit_code().await?,
            Some(0),
            "PostgreSQL TLS fixture setup failed: {:?}",
            configuration_output
        );
        let port = postgres.get_host_port_ipv4(5432).await?;
        let mut database_url = url::Url::parse(&format!(
            "postgres://signalbox:signalbox-test-only@127.0.0.1:{port}/runner_test"
        ))?;
        database_url.query_pairs_mut().append_pair(
            "sslrootcert",
            certificate.to_str().ok_or("fixture path is not UTF-8")?,
        );
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(local_test_connection_options(database_url.as_str())?)
            .await?;
        // Empty fixture files satisfy startup metadata checks; no credential value is used.
        for name in ["absent-model-key", "absent-brave-key", "absent-github-key"] {
            let path = root.join(name);
            fs::write(&path, [])?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        let model_config = root.join("models.toml");
        fs::write(&model_config, model_configuration(root)?)?;
        let templates = root.join("templates.toml");
        fs::write(&templates, "version = 1\n")?;
        let socket = root.join("runner.sock");
        let runner_root = root.join("runner-state");
        let runner_config = root.join("runner.toml");
        let runner_binary = fs::canonicalize(runner_binary())?;
        let configuration = toml::toml! {
            version = 1
            capability_classes = ["echo"]
            tools = ["echo"]
            sandbox_profiles = ["ambient"]
            daemon_socket_path = (socket.to_str().ok_or("socket path is not UTF-8")?)
            runner_root = (runner_root.to_str().ok_or("runner root is not UTF-8")?)
            bubblewrap_path = (runner_binary.to_str().ok_or("runner binary path is not UTF-8")?)
            read_only_paths = ["/usr"]
            allowed_network_hosts = []
            git_author_name = "Runner fixture"
            git_author_email = "runner@example.invalid"
            credentials = {}
            repositories = {}
        };
        fs::write(&runner_config, toml::to_string(&configuration)?)?;
        let daemon_log = fs::File::create(root.join("daemon.log"))?;
        let daemon = Command::new(test_bin_path!("signalboxd"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("DATABASE_URL", database_url.as_str())
            .env("SIGNALBOX_CONFIG_FILE", &model_config)
            .env("SIGNALBOX_TEMPLATE_CONFIG_FILE", templates)
            .env("BRAVE_API_KEY_FILE", root.join("absent-brave-key"))
            .env("GITHUB_TOKEN_FILE", root.join("absent-github-key"))
            .env("SIGNALBOX_SOCKET_PATH", root.join("client.sock"))
            .env("SIGNALBOX_RUNNER_SOCKET_PATH", &socket)
            .env("SIGNALBOX_WEB_BIND", "127.0.0.1:0")
            .env("RUST_LOG", "info")
            .stdout(daemon_log.try_clone()?)
            .stderr(daemon_log)
            .kill_on_drop(true)
            .spawn()?;
        let mut processes = Self {
            pool,
            runner_root,
            runner_config,
            daemon,
            directory,
            _postgres: postgres,
        };
        timeout(PROCESS_ALLOWANCE, async {
            while !socket.exists() {
                if let Some(status) = processes.daemon.try_wait()? {
                    return Err(io::Error::other(format!(
                        "daemon exited {status}: {}",
                        processes.daemon_log()?
                    )));
                }
                sleep(POLL_INTERVAL).await;
            }
            Ok::<_, io::Error>(())
        })
        .await??;
        Ok(processes)
    }

    pub fn spawn_runner(&self) -> Result<Child, Box<dyn Error>> {
        let log = fs::File::create(self.directory.path().join("runner.log"))?;
        Ok(Command::new(runner_binary())
            .env_clear()
            .arg("--config")
            .arg(&self.runner_config)
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()?)
    }

    pub fn daemon_log(&self) -> io::Result<String> {
        fs::read_to_string(self.directory.path().join("daemon.log"))
    }

    pub async fn wait_for_runner(
        &self,
        child: &mut Child,
        message: &str,
    ) -> Result<(), Box<dyn Error>> {
        let observed = timeout(PROCESS_ALLOWANCE, async {
            loop {
                let log = fs::read_to_string(self.directory.path().join("runner.log"))?;
                if log.contains(message) {
                    return Ok::<_, io::Error>(());
                }
                if let Some(status) = child.try_wait()? {
                    return Err(io::Error::other(format!("runner exited {status}: {log}")));
                }
                sleep(POLL_INTERVAL).await;
            }
        })
        .await;
        observed.map_err(|error| {
            io::Error::other(format!(
                "{error}; runner: {}; daemon: {}",
                fs::read_to_string(self.directory.path().join("runner.log")).unwrap_or_default(),
                self.daemon_log().unwrap_or_default()
            ))
        })??;
        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<(), Box<dyn Error>> {
        stop(&mut self.daemon).await?;
        self.pool.close().await;
        Ok(())
    }
}

pub async fn stop(child: &mut Child) -> Result<(), Box<dyn Error>> {
    let pid = child.id().ok_or("child exited before graceful shutdown")?;
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid as i32).ok_or("invalid child PID")?,
        rustix::process::Signal::TERM,
    )?;
    let status = timeout(PROCESS_ALLOWANCE, child.wait()).await??;
    assert!(status.success(), "graceful child shutdown failed: {status}");
    Ok(())
}

fn runner_binary() -> PathBuf {
    // Cargo exposes only this package's binaries; Bazel and nextest supply the
    // companion through their runtime path, while Cargo builds it beside signalboxd.
    resolve(
        "signalbox-runner",
        &test_bin_path!("signalboxd").with_file_name("signalbox-runner"),
    )
}

fn model_configuration(root: &Path) -> Result<String, Box<dyn Error>> {
    let example = include_str!("../../../../config/signalboxd.example.toml")
        .parse::<toml_edit::DocumentMut>()?;
    let mut configuration = (r#"
version = 1
[[credential_profiles]]
name = "fixture-model"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/absent-model-key"
[[credential_pools]]
name = "fixture-models"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "fixture-model", priority = 1 }]
[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "fixture-models"
[compaction]
prompt = "Fixture compaction prompt."
[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "fixture-model"
max_output_tokens = 1024
context_window_tokens = 8192
[[aliases]]
alias_id = "30000000-0000-4000-8000-000000000001"
selection_id = "10000000-0000-4000-8000-000000000001"
"#)
    .parse::<toml_edit::DocumentMut>()?;
    configuration["numeric_bounds"] = example["numeric_bounds"].clone();
    configuration["credential_profiles"][0]["file"] =
        toml_edit::value(root.join("absent-model-key").display().to_string());
    Ok(configuration.to_string())
}
