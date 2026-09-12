use super::*;
use std::{ffi::OsStr, path::PathBuf};

const PURPOSE: &str = "fixture-task-authority";
const VARIABLE: &str = "SIGNALBOX_AMBIENT_FIXTURE_TOKEN";
const SECRET: &str = "synthetic-ambient-\"secret\"";

#[derive(Clone, Debug)]
struct AmbientRunner {
    requests: Arc<Mutex<Vec<ProcessRequest>>>,
    pool: PgPool,
    destination: PathBuf,
}

impl ProcessRunner for AmbientRunner {
    fn sandbox_launcher_program(&self) -> &std::path::Path {
        std::path::Path::new(OFFLINE_SANDBOX_LAUNCHER)
    }

    fn sandbox_launcher_descriptor(&self) -> Option<i32> {
        Some(OFFLINE_SANDBOX_LAUNCHER_DESCRIPTOR)
    }

    async fn bwrap_availability(&mut self, probe: ProcessRequest) -> BwrapAvailability {
        assert_eq!(
            std::path::Path::new(&probe.program).file_name(),
            Some(OsStr::new("bwrap"))
        );
        BwrapAvailability::Available
    }

    async fn run(&mut self, request: ProcessRequest) -> ProcessRunResult {
        assert_eq!(
            std::path::Path::new(&request.program).file_name(),
            Some(OsStr::new("bwrap"))
        );
        assert!(request.arguments.contains(&"--unshare-net".into()));
        assert!(!request.environment.contains_key(OsStr::new(VARIABLE)));
        let mount = request
            .arguments
            .windows(3)
            .find(|args| args[0] == "--ro-bind" && args[2] == self.destination.as_os_str());
        let environment = request
            .arguments
            .windows(3)
            .find(|args| args[0] == "--setenv" && args[1] == VARIABLE);
        let secret = if let Some(mount) = mount {
            assert_ne!(mount[1], self.destination.as_os_str());
            fs::read(&mount[1]).expect("task snapshot exists during execution")
        } else if let Some(environment) = environment {
            environment[2].as_encoded_bytes().to_vec()
        } else {
            Vec::new()
        };
        if !secret.is_empty() {
            assert_eq!(secret, SECRET.as_bytes());
            let approved: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM tool_approval_decision
                 WHERE decision_source = 'delegate' AND decision_kind = 'approve')",
            )
            .fetch_one(&self.pool)
            .await
            .expect("judge provenance recorded before execution");
            assert!(approved);
        }
        assert!(!format!("{request:?}").contains(SECRET));
        tracing::info!(?request, "ambient task process boundary");
        self.requests.lock().expect("requests").push(request);
        ProcessRunResult {
            outcome: ProcessOutcome::Exited { code: Some(0) },
            stdout: ProcessOutput {
                bytes: secret.clone(),
                completeness: CaptureCompleteness::Complete,
            },
            stderr: ProcessOutput {
                // Fixture for the launcher dispatch wire marker.
                bytes: [b"signalbox-exec:dispatched\n".as_slice(), secret.as_slice()].concat(),
                completeness: CaptureCompleteness::Complete,
            },
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn judge_gates_each_ambient_task_and_credentials_end_with_the_task()
-> Result<(), Box<dyn Error>> {
    if std::env::var(VARIABLE).as_deref() != Ok(SECRET) {
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "ambient_credentials::judge_gates_each_ambient_task_and_credentials_end_with_the_task", "--ignored", "--nocapture"])
            .env(VARIABLE, SECRET)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }
    let log = tempfile::NamedTempFile::new()?;
    let writer = log.reopen()?;
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.try_clone().expect("log writer"))
            .finish(),
    )?;
    for delivery in ["file", "variable"] {
        for recommendation in ["approve", "deny"] {
            let fixture = ToolLoopFixture::new(DangerousToolAutoApproval::ApproveAll).await?;
            let workspace = tempdir()?;
            git2::Repository::init(workspace.path())?;
            let credential = tempfile::NamedTempFile::new()?;
            fs::write(credential.path(), SECRET)?;
            let source = if delivery == "file" {
                credential.path().to_str().expect("path")
            } else {
                VARIABLE
            };
            let models = support::parse_model_configuration(&format!(
                "{}\n[[credential_profiles]]\nname = {PURPOSE:?}\nadapter = \"sandboxed_exec\"\ndelivery = \"ambient\"\n{delivery} = {source:?}\n",
                approval_judge_model_configuration_source("10m")
            ))?;
            let catalogs = signalboxd::configuration_reload::ConfigurationReload::new(
                fixture.pool.clone(),
                models.clone(),
                Default::default(),
                workspace.path().join("models.toml"),
                workspace.path().join("templates.toml"),
                None,
            )
            .expect("catalogs");
            let runner = AmbientRunner {
                requests: Default::default(),
                pool: fixture.pool.clone(),
                destination: credential.path().to_owned(),
            };
            let (catalog, executor) = DaemonTools::try_new(
                (|| SystemTime::UNIX_EPOCH) as fn() -> SystemTime,
                OfflineWebTransport::unused(),
                MappedDaemonCredentialInputs {
                    web_search: OfflineCodeHostCredentials,
                    code_host: OfflineCodeHostCredentials,
                    github: OfflineCodeHostCredentials,
                },
                UnusedWebSearchTransport,
                UnusedSessionStatusWriter,
                UnusedCodeHostTransport,
                UnusedGitHubTransport,
                GitHubEgressPolicy::github_api_only(),
                LocalWorkspaceFileSystem,
                workspace.path(),
                git_identity(),
                runner.clone(),
                PostgresConversationIntrospection::new(fixture.pool.clone()),
                signalbox_persistence::plan::SessionPlanRepository::new(fixture.pool.clone()),
                WebFetchEgressPolicy::deny_all(),
            )?
            .into_parts();
            let executor = executor.with_ambient_credentials(catalogs, fixture.pool.clone());
            let arguments =
                serde_json::json!({"program":"fixture-command","credential_purpose":PURPOSE})
                    .to_string();
            let (execution, runtime, judge) = fixture.execution_with_judge_configuration(
                [
                    tool_use_script(&[(SANDBOXED_EXEC_NAME, &arguments)]),
                    tool_use_script(&[(SANDBOXED_EXEC_NAME, r#"{"program":"fixture-command"}"#)]),
                    completion_script("tasks observed"),
                ],
                approval_judge_script(
                    recommendation,
                    "The exact credential purpose was evaluated.",
                ),
                catalog,
                executor,
                models,
            );
            execution
                .execute(Box::new(fixture.activated.clone()))
                .await?;
            let requests = runner.requests.lock().expect("requests");
            assert_eq!(
                requests.len(),
                if recommendation == "approve" { 2 } else { 1 }
            );
            let ordinary = requests.last().expect("ordinary task executes");
            assert!(
                !ordinary
                    .arguments
                    .iter()
                    .any(|value| value == credential.path().as_os_str() || value == VARIABLE)
            );
            for request in requests.iter() {
                if let Some(mount) = request
                    .arguments
                    .windows(3)
                    .find(|args| args[0] == "--ro-bind" && args[2] == credential.path().as_os_str())
                {
                    assert!(
                        !std::path::Path::new(&mount[1]).exists(),
                        "task snapshot removed"
                    );
                }
            }
            drop(requests);
            assert_eq!(
                judge.received_operations().len(),
                1,
                "the blanket must still consult the judge"
            );
            let judge_input = format!("{:?}", judge.received_operations());
            assert!(judge_input.contains(PURPOSE));
            assert!(judge_input.contains("credential_purpose"));
            assert!(!judge_input.contains(SECRET));
            assert!(!format!("{:?}", runtime.received_operations()).contains(SECRET));
            if recommendation == "approve" {
                let result = continuation_result_json(&runtime)?;
                assert_eq!(result["confinement"]["kind"], "filesystem_confined");
                assert_eq!(result["stdout"]["text"], "[redacted]");
                assert_eq!(result["stderr"]["text"], "[redacted]");
            }
        }
    }
    let logs = fs::read_to_string(log.path())?;
    assert!(logs.contains("ambient task process boundary"));
    assert!(!logs.contains(SECRET));
    Ok(())
}
