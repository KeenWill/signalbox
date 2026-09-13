//! The production tool loop and socket runtime execute the packaged runner's echo child.

use super::*;
use signalbox_domain::{
    RunnerCapabilityClass, RunnerSandboxProfile, RunnerSelector, RunnerToolPermissionOverrides,
    RunnerWorkingDirectory, SessionRunnerPlacementRequest, SessionRunnerPlacementState,
    WorkingDirectorySelection, WorkspaceRequirement,
};
use signalboxd::runner_protocol_runtime::{
    PostgresRunnerRegistrationService, RunnerProtocolRuntime,
};
use std::path::PathBuf;
use tokio::process::{Child, Command};

struct RunnerHost {
    root: tempfile::TempDir,
    child: Child,
    shutdown: watch::Sender<bool>,
    server: tokio::task::JoinHandle<
        Result<(), signalboxd::runner_protocol_runtime::RunnerProtocolRuntimeError>,
    >,
    service: PostgresRunnerRegistrationService,
}

impl RunnerHost {
    async fn start(pool: PgPool) -> Result<Self, Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
        let socket = root.path().join("runner.sock");
        let binary = fs::canonicalize(signalbox_test_bin::resolve(
            "signalbox-runner",
            &signalbox_test_bin::test_bin_path!("signalboxd").with_file_name("signalbox-runner"),
        ))?;
        let service =
            PostgresRunnerRegistrationService::local(pool).expect("compiled runner catalog");
        let listener = signalboxd::LocalProcessListener::bind(&socket)?;
        let (shutdown, receiver) = watch::channel(false);
        let server =
            tokio::spawn(RunnerProtocolRuntime::new(listener, service.clone()).run(receiver));
        let configuration = root.path().join("runner.toml");
        let runner_root = root.path().join("state");
        let mut document = toml::toml! {
            version = 1
            capability_classes = ["echo"]
            tools = ["echo"]
            sandbox_profiles = ["ambient"]
            daemon_socket_path = (socket.to_str().expect("fixture path"))
            runner_root = (runner_root.to_str().expect("fixture path"))
            bubblewrap_path = (binary.to_str().expect("packaged binary path"))
            read_only_paths = ["/usr"]
            allowed_network_hosts = []
            git_author_name = "Runner fixture"
            git_author_email = "runner@example.invalid"
            credentials = {}
            repositories = {}
        };
        document["credentials"] = toml::Value::Table(toml::Table::from_iter([(
            "github-runner".to_owned(),
            toml::Value::Table(toml::Table::from_iter([
                (
                    "file".to_owned(),
                    toml::Value::String(
                        root.path()
                            .join("absent-credential")
                            .to_str()
                            .expect("fixture path")
                            .to_owned(),
                    ),
                ),
                (
                    "injection_env".to_owned(),
                    toml::Value::String("GH_TOKEN".to_owned()),
                ),
            ])),
        )]));
        fs::write(&configuration, toml::to_string(&document)?)?;
        let log = fs::File::create(root.path().join("runner.log"))?;
        let child = Command::new(binary)
            .env_clear()
            .arg("--config")
            .arg(configuration)
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()?;
        let mut host = Self {
            root,
            child,
            shutdown,
            server,
            service,
        };
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let log = fs::read_to_string(host.root.path().join("runner.log"))?;
                if log.contains("runner enrolled") {
                    return Ok::<_, Box<dyn Error>>(());
                }
                if let Some(status) = host.child.try_wait()? {
                    return Err(format!("runner exited {status}: {log}").into());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        Ok(host)
    }

    async fn stop(mut self) -> Result<(), Box<dyn Error>> {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let journal: serde_json::Value = serde_json::from_slice(&fs::read(
                    self.root.path().join("state/operation-journal.json"),
                )?)?;
                if journal["journal"]["entries"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
                {
                    return Ok::<_, Box<dyn Error>>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        let pid = self.child.id().expect("live packaged runner");
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid as i32).expect("child PID"),
            rustix::process::Signal::TERM,
        )?;
        assert!(
            tokio::time::timeout(Duration::from_secs(60), self.child.wait())
                .await??
                .success()
        );
        let state = signalbox_runner::RunnerStateRoot::open(&self.root.path().join("state"))?;
        assert_eq!(
            state.reconnect_inventory(),
            signalbox_runner_wire::ReconnectInventory::default(),
            "result acknowledgement releases the runner's durable slot"
        );
        drop(state);
        self.shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(60), self.server).await???;
        Ok(())
    }
}

fn placement(directory: PathBuf) -> SessionRunnerPlacementRequest {
    SessionRunnerPlacementRequest {
        selector: RunnerSelector::CapabilityClass(
            RunnerCapabilityClass::try_new("echo".to_owned()).expect("compiled class"),
        ),
        working_directory: WorkingDirectorySelection::Exact(
            RunnerWorkingDirectory::try_new(
                directory.to_str().expect("fixture directory").to_owned(),
            )
            .expect("exact fixture directory"),
        ),
        credential_profile: None,
        workspace: WorkspaceRequirement::None,
        sandbox: RunnerSandboxProfile::Ambient,
        permission_overrides: RunnerToolPermissionOverrides::try_new([]).expect("no overrides"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and the packaged runner"]
async fn placed_echo_completes_through_the_packaged_runner() -> Result<(), Box<dyn Error>> {
    check_placed_echo(false, false).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and the packaged runner"]
async fn an_ineligible_connected_runner_preserves_daemon_echo_fallback()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let mut requested = placement(directory.path().to_owned());
    requested.selector =
        RunnerSelector::Identity(signalbox_domain::RunnerId::from_uuid(Uuid::now_v7()));
    let fixture = ToolLoopFixture::with_creation_placement(
        DangerousToolAutoApproval::Disabled,
        None,
        migrated_postgres().await?,
        Some(requested),
    )
    .await?;
    let host = RunnerHost::start(fixture.pool.clone()).await?;
    let dispatch = host.service.dispatch_service();
    let (catalog, executor) = offline_daemon_tools(
        OfflineWebTransport::unused(),
        UnusedSessionStatusWriter,
        UnusedCodeHostTransport,
        WebFetchEgressPolicy::deny_all(),
    )?
    .into_parts();
    let arguments = serde_json::json!({"text": "daemon fallback"}).to_string();
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&[("echo", arguments.as_str())]),
            completion_script("observed"),
        ],
        catalog,
        executor.with_runner_dispatch(dispatch.clone()),
    );
    tokio::time::timeout(
        Duration::from_secs(60),
        execution
            .with_runner_dispatch(dispatch)
            .execute(Box::new(fixture.activated.clone())),
    )
    .await??;
    let request = fixture.wait_for_requests(1).await?[0];
    assert_eq!(
        continuation_tool_exchange(&runtime)?,
        vec![
            expected_tool_call(request, "echo", &arguments),
            expected_successful_tool_result(request, arguments)
        ]
    );
    assert_eq!(
        host.service
            .recovery_store()
            .load_placement(fixture.session)
            .await?
            .expect("requested placement")
            .placement()
            .state(),
        &SessionRunnerPlacementState::Unpinned
    );
    host.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and the packaged runner"]
async fn a_lost_claim_releases_the_live_tool_loop_into_durable_runner_recovery()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::runner_protocol::RunnerConnectionTransition;
    use signalboxd::runner_protocol_runtime::RunnerRegistrationService as _;
    let directory = tempfile::tempdir()?;
    let fixture = ToolLoopFixture::with_creation_placement(
        DangerousToolAutoApproval::Disabled,
        None,
        migrated_postgres().await?,
        Some(placement(directory.path().to_owned())),
    )
    .await?;
    let mut host = RunnerHost::start(fixture.pool.clone()).await?;
    let live = host
        .service
        .recovery_store()
        .load_nonterminal_connection_heads()
        .await?;
    let live = &live[0];
    let enrollment = live.enrollment();
    let epoch = live.epoch();
    let pid = rustix::process::Pid::from_raw(host.child.id().expect("runner PID") as i32)
        .expect("positive PID");
    rustix::process::kill_process(pid, rustix::process::Signal::STOP)?;
    let dispatch = host.service.dispatch_service();
    let (catalog, executor) = offline_daemon_tools(
        OfflineWebTransport::unused(),
        UnusedSessionStatusWriter,
        UnusedCodeHostTransport,
        WebFetchEgressPolicy::deny_all(),
    )?
    .into_parts();
    let arguments = serde_json::json!({"text": "must await recovery"}).to_string();
    let (execution, runtime) = fixture.execution(
        [tool_use_script(&[("echo", arguments.as_str())])],
        catalog,
        executor.with_runner_dispatch(dispatch.clone()),
    );
    let execution = execution.with_runner_dispatch(dispatch);
    let lose = async {
        let lease = loop {
            if let Some(lease) = host
                .service
                .recovery_store()
                .pending_tool_lease(enrollment, epoch)
                .await?
            {
                break lease;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        host.service
            .recovery_store()
            .claim_tool_lease(enrollment, epoch, lease.correlation())
            .await?;
        host.service
            .transition_connection(
                signalbox_runner_wire::CanonicalUuid::from_uuid(enrollment.into_uuid()),
                signalbox_runner_wire::PositiveU64::try_new(epoch.get())?,
                RunnerConnectionTransition::TransportClosed,
            )
            .await?;
        Ok::<_, Box<dyn Error>>(())
    };
    let outcome = tokio::time::timeout(Duration::from_secs(60), async {
        let (executed, lost) =
            tokio::join!(execution.execute(Box::new(fixture.activated.clone())), lose);
        lost?;
        executed?;
        Ok::<_, Box<dyn Error>>(())
    })
    .await;
    host.child.kill().await?;
    host.shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(60), host.server).await???;
    outcome??;
    assert!(
        host.service
            .recovery_store()
            .load_runner_recovery_wait(fixture.session)
            .await?
            .is_some()
    );
    assert_eq!(
        runtime.received_operations().len(),
        1,
        "loss cannot request model continuation"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and the packaged runner"]
async fn named_profile_echo_creates_a_grant_without_resolving_its_credential()
-> Result<(), Box<dyn Error>> {
    check_placed_echo(true, false).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and the packaged runner"]
async fn runner_confirm_requires_human_approval_despite_the_daemon_blanket()
-> Result<(), Box<dyn Error>> {
    check_placed_echo(false, true).await
}

async fn check_placed_echo(named_profile: bool, confirm: bool) -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let mut requested = placement(directory.path().to_owned());
    if named_profile {
        requested.credential_profile = Some(
            signalbox_domain::CredentialProfileName::try_new("github-runner".to_owned())
                .expect("compiled credential profile"),
        );
    }
    if confirm {
        requested.permission_overrides = RunnerToolPermissionOverrides::try_new([(
            signalbox_domain::ToolName::try_new("echo".to_owned()).expect("compiled pure tool"),
            signalbox_domain::RunnerToolPermissionOverride::Confirm,
        )])
        .expect("one runner confirmation override");
    }
    let fixture = ToolLoopFixture::with_creation_placement(
        if confirm {
            DangerousToolAutoApproval::ApproveAll
        } else {
            DangerousToolAutoApproval::Disabled
        },
        None,
        migrated_postgres().await?,
        Some(requested),
    )
    .await?;
    let host = RunnerHost::start(fixture.pool.clone()).await?;
    let dispatch = host.service.dispatch_service();
    let web = OfflineWebTransport::unused();
    let (catalog, executor) = offline_daemon_tools(
        web.clone(),
        UnusedSessionStatusWriter,
        UnusedCodeHostTransport,
        WebFetchEgressPolicy::deny_all(),
    )?
    .into_parts();
    let arguments = serde_json::json!({"text": "packaged runner echo"}).to_string();
    let calls = if named_profile {
        vec![("echo", arguments.as_str()), ("echo", arguments.as_str())]
    } else {
        vec![("echo", arguments.as_str())]
    };
    let (execution, runtime) = fixture.execution(
        [
            tool_use_script(&calls),
            completion_script("runner observed"),
        ],
        catalog,
        executor.with_runner_dispatch(dispatch.clone()),
    );
    let execution = execution.with_runner_dispatch(dispatch);

    tokio::time::timeout(
        Duration::from_secs(60),
        execution.execute(Box::new(fixture.activated.clone())),
    )
    .await??;

    let requests = fixture.wait_for_requests(calls.len()).await?;
    let request = requests[0];
    if confirm {
        let before = host
            .service
            .recovery_store()
            .load_placement(fixture.session)
            .await?
            .expect("requested placement");
        assert_eq!(
            before.placement().state(),
            &SessionRunnerPlacementState::Unpinned
        );
        assert!(before.grant().is_none());
        fixture
            .decide(request, ToolApprovalDecision::Approve)
            .await?;
        tokio::time::timeout(
            Duration::from_secs(60),
            execution.resume_active(fixture.session),
        )
        .await??;
    }
    assert_eq!(
        continuation_tool_exchange(&runtime)?,
        requests
            .iter()
            .map(|request| expected_tool_call(*request, "echo", &arguments))
            .chain(
                requests
                    .iter()
                    .map(|request| expected_successful_tool_result(*request, arguments.clone()))
            )
            .collect::<Vec<_>>()
    );
    let placement = host
        .service
        .recovery_store()
        .load_placement(fixture.session)
        .await?
        .expect("initial pin retained");
    assert!(
        matches!(
            placement.placement().state(),
            SessionRunnerPlacementState::Pinned(_)
        ),
        "the result came from a durably pinned runner"
    );
    assert_eq!(placement.grant().is_some(), named_profile);
    assert_eq!(
        placement
            .registration()
            .expect("registration snapshot")
            .registration()
            .tools()
            .map(|tool| tool.name().as_str())
            .collect::<Vec<_>>(),
        ["echo"]
    );
    assert!(web.requests().is_empty());
    let status =
        signalbox_persistence::runner_protocol::status::read_runner_status(&fixture.pool, 10, None)
            .await?;
    assert!(status.runners.iter().any(|fact| matches!(
        fact,
        signalbox_persistence::runner_protocol::status::RunnerStatusFact::Enrollment {
            connection: Some(
                signalbox_persistence::runner_protocol::RunnerConnectionState::Connected
            ),
            ..
        }
    )));
    assert!(status.runners.iter().any(|fact| matches!(fact,
        signalbox_persistence::runner_protocol::status::RunnerStatusFact::Placement { session, runner }
            if *session == fixture.session && runner.state() == signalbox_persistence::process_read::ProcessRunnerProjectionState::Pinned
                && runner.connection_health() == Some(signalbox_persistence::process_read::ProcessRunnerConnectionHealth::Connected)
    )));
    host.stop().await?;
    Ok(())
}
