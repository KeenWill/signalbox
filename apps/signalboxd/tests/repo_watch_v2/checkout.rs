use super::*;
use signalbox_application::{InProcessEligibilityWorkSource, InProcessToolDispatchGate};
use signalbox_persistence::scheduler::PostgresEligibilitySweep;
use signalbox_tools_exec::{
    BwrapAvailability, CaptureCompleteness, ProcessEnvironment, ProcessOutcome, ProcessOutput,
    ProcessRequest, ProcessRunResult, ProcessRunner,
};
use signalboxd::{
    HubModelConfiguration, SessionWorkspaceRoots,
    repo_watch_dispatch::{
        RepositoryWatchCommandCodec, RepositoryWatchCommandSink, submit_pending_with_runner,
    },
};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const TOKEN: &str = "checkout-fixture-token";

/// Replaces only the fixture GitHub transport with a real local bare repository.
#[derive(Clone)]
struct LocalGitRunner {
    bare: PathBuf,
    redirect: Option<String>,
    steps: Arc<Mutex<Vec<String>>>,
}

impl ProcessRunner for LocalGitRunner {
    fn sandbox_launcher_program(&self) -> &Path {
        Path::new("/unused-checkout-fixture-launcher")
    }
    fn sandbox_launcher_descriptor(&self) -> Option<i32> {
        None
    }
    async fn bwrap_availability(&mut self, _: ProcessRequest) -> BwrapAvailability {
        BwrapAvailability::Missing
    }
    async fn run(&mut self, mut request: ProcessRequest) -> ProcessRunResult {
        assert_eq!(request.program, "git");
        assert_eq!(request.environment_inheritance, ProcessEnvironment::Clear);
        assert!(
            request
                .arguments
                .iter()
                .all(|argument| !argument.to_string_lossy().contains(TOKEN))
        );
        assert!(
            request
                .environment
                .contains_key(OsStr::new("GIT_CONFIG_VALUE_0"))
        );
        self.steps
            .lock()
            .expect("steps lock")
            .push(request.arguments[0].to_string_lossy().into_owned());
        for argument in &mut request.arguments {
            if argument == "https://github.com/checkout/project.git"
                || argument == "https://github.com/contributor/project.git"
            {
                let authorization = tokio::process::Command::new("git")
                    .args(["config", "--get-urlmatch", "http.extraheader"])
                    .arg(&argument)
                    .current_dir(&request.working_directory)
                    .env_clear()
                    .envs(&request.environment)
                    .output()
                    .await
                    .expect("resolve Git URL authorization");
                if argument == "https://github.com/checkout/project.git" {
                    assert!(authorization.status.success());
                    assert!(authorization.stdout.starts_with(b"Authorization: Basic "));
                    let unrelated = tokio::process::Command::new("git")
                        .args([
                            "config",
                            "--get-urlmatch",
                            "http.extraheader",
                            "https://github.com/unrelated/project.git",
                        ])
                        .current_dir(&request.working_directory)
                        .env_clear()
                        .envs(&request.environment)
                        .output()
                        .await
                        .expect("resolve unrelated repository authorization");
                    assert!(unrelated.stdout.is_empty());
                } else {
                    assert!(authorization.stdout.is_empty());
                    assert_eq!(
                        request.environment.get(OsStr::new("GIT_CONFIG_VALUE_0")),
                        Some(&"".into())
                    );
                }
                if let Some(url) = &self.redirect {
                    *argument = url.into();
                    request.environment.insert(
                        "GIT_CONFIG_KEY_0".into(),
                        format!("http.{url}.extraheader").into(),
                    );
                } else {
                    *argument = self.bare.clone().into_os_string();
                }
            }
        }
        let output = tokio::process::Command::new(&request.program)
            .args(&request.arguments)
            .current_dir(&request.working_directory)
            .env_clear()
            .envs(&request.environment)
            .output()
            .await
            .expect("run fixture Git");
        ProcessRunResult {
            outcome: ProcessOutcome::Exited {
                code: output.status.code(),
            },
            stdout: ProcessOutput {
                bytes: output.stdout,
                completeness: CaptureCompleteness::Complete,
            },
            stderr: ProcessOutput {
                bytes: output.stderr,
                completeness: CaptureCompleteness::Complete,
            },
        }
    }
}

struct CheckoutFixture {
    _container: ContainerAsync<Postgres>,
    _files: tempfile::TempDir,
    core: PgPool,
    module: PgPool,
    store: RepoWatchStore,
    sink: RepositoryWatchCommandSink,
    runner: LocalGitRunner,
    command: DurableCommandId,
    head: CommitSha,
    catalog: String,
}

impl CheckoutFixture {
    async fn new() -> Result<Self, Box<dyn Error>> {
        Self::with_head_repository("checkout/project").await
    }

    async fn with_head_repository(head_repository: &str) -> Result<Self, Box<dyn Error>> {
        let (container, core, url) = postgres().await?;
        migrate(&core).await?;
        sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
            .execute(&core)
            .await?;
        let module = module_pool(&url).await?;
        let store = RepoWatchStore::new(module.clone());
        let files = tempfile::tempdir()?;
        let source_path = files.path().join("source");
        let source = git2::Repository::init(&source_path)?;
        std::fs::write(source_path.join("review.txt"), "retained head\n")?;
        let mut index = source.index()?;
        index.add_path(Path::new("review.txt"))?;
        index.write()?;
        let tree_id = index.write_tree()?;
        let signature = git2::Signature::now("Checkout fixture", "checkout@example.test")?;
        let sha = source.commit(
            Some("refs/heads/review"),
            &signature,
            &signature,
            "Recorded head",
            &source.find_tree(tree_id)?,
            &[],
        )?;
        source.set_head("refs/heads/review")?;
        let bare = files.path().join("remote.git");
        git2::build::RepoBuilder::new()
            .bare(true)
            .clone(source_path.to_str().expect("UTF-8 fixture path"), &bare)?;
        let root = files.path().join("workspace");
        git2::Repository::init(&root)?;
        let credential = files.path().join("poll-token");
        std::fs::write(&credential, TOKEN)?;
        let catalog = include_str!("../../../../config/signalboxd.example.toml")
            .replace(
                "/usr/local/bin/signalbox-exec-supervisor",
                std::env::current_exe()?.to_str().expect("test executable"),
            )
            .replace(
                "/srv/signalbox/workspace",
                root.to_str().expect("fixture root"),
            );
        let catalog = format!(
            r#"{catalog}
[repository_watch]
version = 1
enabled = true
signal_reviewers = []
[[repository_watch.repositories]]
repository = "checkout/project"
poll_interval_seconds = 60
credential_file = "{}"
[[repository_watch.rules]]
id = "review"
version = 1
singleton_per = "pull_request"
cooldown_seconds = 0
[repository_watch.rules.matcher]
event_kinds = ["pull_request_opened"]
[[repository_watch.rules.actions]]
kind = "dispatch_session"
template = "watch"
"#,
            credential.display()
        );
        let models = HubModelConfiguration::parse(&catalog)?;
        let configuration = models
            .repository_watch()
            .expect("repository watch configuration");
        let repository = configuration.repositories()[0].repository();
        let now = OffsetDateTime::now_utc();
        store
            .reconcile_rules(
                &[RepositoryRuleSet::new(repository, configuration.rules())],
                now,
            )
            .await?;
        let head = CommitSha::try_new(sha.to_string())?;
        let observation = RepoWatchObservation::new(
            Vec::new(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
                pull_requests: vec![ComparisonPullRequestState::try_new(
                    RepoWatchPullRequestStateInput {
                        context: PullRequestEventContext::new(PullRequestEventContextInput {
                            number: PullRequestNumber::new(
                                NonZeroU64::new(1).expect("positive PR number"),
                            ),
                            head_sha: head.clone(),
                            head_repository: RepositorySlug::try_new(head_repository.to_owned())?,
                            base_branch: BranchName::try_new(String::from("main"))?,
                            head_branch: BranchName::try_new(String::from("review"))?,
                            title: PullRequestTitle::try_new(String::from(
                                "Review the retained head",
                            ))?,
                            body: PullRequestBody::try_new(String::new())?,
                            labels: Vec::new(),
                            draft: false,
                            author: None,
                        }),
                        lifecycle: RepoWatchPullRequestLifecycle::Open,
                        mergeable_state: MergeableState::Unknown,
                        completed_check_suites: Vec::new(),
                        completed_check_runs: Vec::new(),
                        reviews: Vec::new(),
                        threads: Vec::new(),
                        reactions: Vec::new(),
                    },
                )?],
                ..RepoWatchRepositoryStateInput::default()
            })?,
        );
        store
            .ingest_observation(
                &store.ingest_baseline(repository).await?,
                &signalbox_module_repo_watch_v2::ingest::RepositoryObservation {
                    repository: repository.clone(),
                    default_branch: BranchName::try_new(String::from("main"))?,
                    default_head: head.clone(),
                    observed_at: now,
                    observation,
                },
                EventProducer::Poll,
            )
            .await?;
        let template_path = files.path().join("templates.toml");
        std::fs::write(
            &template_path,
            r#"version = 1
[[templates]]
name = "watch"
version = 1
alias = "540ce009-c2ec-4a04-b823-c411ea189778"
dangerous_tool_auto_approval = false
system_prompt = "Inspect repository activity."
"#,
        )?;
        let templates =
            signalboxd::SessionTemplateConfiguration::read(&template_path, || None, &models)?;
        let mut factory =
            signalboxd::repo_watch_dispatch::RepositoryWatchCommandFactory(Arc::new(templates));
        assert!(
            store
                .evaluate_next(
                    repository,
                    &configuration.rules()[0],
                    &mut FixedDispatchIds {
                        value: 43001,
                        calls: 0
                    },
                    &mut factory,
                    &mut RepositoryWatchCommandCodec,
                    now
                )
                .await
                .expect("evaluate PR dispatch")
        );
        let command = store
            .recover_pending_commands(&mut RepositoryWatchCommandCodec)
            .await?[0]
            .command()
            .command_id();
        let (eligibility_nudge, _) =
            InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(core.clone()));
        Ok(Self {
            _container: container,
            _files: files,
            core: core.clone(),
            module,
            store,
            sink: RepositoryWatchCommandSink {
                pool: core,
                models: Arc::new(models),
                eligibility_nudge,
                tool_dispatch_gate: InProcessToolDispatchGate::default(),
            },
            runner: LocalGitRunner {
                bare,
                redirect: None,
                steps: Arc::default(),
            },
            command,
            head,
            catalog,
        })
    }

    async fn submit_without_lifecycle_settlement(&mut self) {
        let configuration = self
            .sink
            .models
            .repository_watch()
            .expect("configuration")
            .clone();
        submit_pending_with_runner(
            &self.store,
            &configuration,
            &mut self.sink,
            self.runner.clone(),
        )
        .await
        .expect("dispatch with checkout");
    }

    async fn dispatch(&mut self) {
        self.submit_without_lifecycle_settlement().await;
        let lifecycle = signalbox_ownership_seam::LifecycleEventSource::new(self.core.clone());
        while let Some(event) = lifecycle.next().await.expect("next lifecycle event") {
            self.store
                .apply_lifecycle_event(&event)
                .await
                .expect("settle ledger from lifecycle event");
            lifecycle
                .acknowledge(&event)
                .await
                .expect("acknowledge lifecycle event");
        }
    }

    async fn session(&self) -> SessionId {
        let id: Uuid = sqlx::query_scalar(
            "SELECT created_session_id FROM dispatch_ledger WHERE command_id = $1",
        )
        .bind(self.command.into_uuid())
        .fetch_one(&self.module)
        .await
        .expect("created session");
        SessionId::from_uuid(id)
    }

    fn root(&self, session: SessionId) -> PathBuf {
        SessionWorkspaceRoots::try_new(
            self.sink
                .models
                .daemon_tools()
                .expect("tools")
                .workspace_root(),
        )
        .expect("derived roots")
        .derived_path(session)
    }

    fn change_workspace_root(&mut self) -> Result<(), Box<dyn Error>> {
        let replacement = self._files.path().join("replacement-workspace");
        git2::Repository::init(&replacement)?;
        let previous = self
            .sink
            .models
            .daemon_tools()
            .expect("tools")
            .workspace_root();
        self.sink.models = Arc::new(HubModelConfiguration::parse(&self.catalog.replace(
            previous.to_str().expect("fixture workspace"),
            replacement.to_str().expect("replacement workspace"),
        ))?);
        Ok(())
    }

    async fn stop(&mut self, session: SessionId) {
        use signalbox_module_repo_watch_v2::dispatch::SessionCommandSink;
        self.sink
            .submit(
                SessionCommand::lifecycle(SessionLifecycleCommand::new(
                    DurableCommandId::from_uuid(Uuid::now_v7()),
                    session,
                    SessionLifecycleOperation::Stop {
                        sticky: StopStickiness::Sticky,
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                ))
                .expect("stop admitted by seam"),
            )
            .await
            .expect("stop session");
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn authenticated_clone_refuses_a_same_origin_repository_redirect()
-> Result<(), Box<dyn Error>> {
    use axum::{http::StatusCode, response::IntoResponse};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let location = format!("{origin}/transferred/project.git/info/refs?service=git-upload-pack");
    let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let recorded = recorded.clone();
        let location = location.clone();
        async move {
            recorded.lock().expect("requests").push((
                request.uri().path().to_owned(),
                request.headers().contains_key("authorization"),
            ));
            if request.uri().path().starts_with("/checkout/") {
                (StatusCode::FOUND, [("location", location)]).into_response()
            } else {
                StatusCode::UNAUTHORIZED.into_response()
            }
        }
    });
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let mut fixture = CheckoutFixture::new().await?;
    fixture.runner.redirect = Some(format!("{origin}/checkout/project.git"));
    fixture.dispatch().await;
    server.abort();
    assert_eq!(
        *requests.lock().expect("requests"),
        [(String::from("/checkout/project.git/info/refs"), true)],
    );
    let failure: (String, String) = sqlx::query_as(
        "SELECT checkout_retired_reason, checkout_failure_step FROM dispatch_ledger WHERE command_id = $1",
    ).bind(fixture.command.into_uuid()).fetch_one(&fixture.module).await?;
    assert_eq!(
        failure,
        (
            String::from("checkout_provisioning_failed"),
            String::from("clone")
        )
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn replay_preserves_a_provisioned_session_after_repository_removal()
-> Result<(), Box<dyn Error>> {
    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    sqlx::query("UPDATE dispatch_ledger SET submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid())
        .execute(&fixture.module)
        .await?;
    fixture.sink.models = Arc::new(HubModelConfiguration::parse(
        &fixture.catalog.replace("checkout/project", "other/project"),
    )?);
    std::fs::remove_file(fixture._files.path().join("poll-token"))?;
    fixture.store = RepoWatchStore::new(fixture.module.clone());
    fixture.dispatch().await;
    let ledger: (String, Option<String>, bool) = sqlx::query_as(
        "SELECT checkout_head_sha, checkout_retired_reason, submission_pending FROM dispatch_ledger WHERE command_id = $1",
    ).bind(fixture.command.into_uuid()).fetch_one(&fixture.module).await?;
    assert_eq!(ledger, (fixture.head.as_str().to_owned(), None, false));
    let state: (bool, bool) = sqlx::query_as(
        "SELECT state_kind = 'terminal', start_gate_held FROM session_lifecycle WHERE session_id = $1",
    ).bind(session.into_uuid()).fetch_one(&fixture.core).await?;
    assert_eq!(state, (false, true));
    assert_eq!(
        *fixture.runner.steps.lock().expect("steps"),
        ["clone", "fetch", "checkout"]
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn recovery_terminalizes_an_unconfigured_repository_without_a_sticky_stop()
-> Result<(), Box<dyn Error>> {
    let mut fixture = CheckoutFixture::new().await?;
    let original_models = fixture.sink.models.clone();
    fixture.sink.models = Arc::new(HubModelConfiguration::parse(
        &fixture.catalog.replace("checkout/project", "other/project"),
    )?);
    let configuration = fixture
        .sink
        .models
        .repository_watch()
        .expect("configuration");
    fixture
        .store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                configuration.repositories()[0].repository(),
                configuration.rules(),
            )],
            OffsetDateTime::now_utc(),
        )
        .await?;
    std::fs::remove_file(fixture._files.path().join("poll-token"))?;
    fixture.store = RepoWatchStore::new(fixture.module.clone());
    fixture.dispatch().await;
    let session = fixture.session().await;
    let disposition: (String, String, String, bool, Uuid) = sqlx::query_as(
        "SELECT repository, status, checkout_retired_reason, submission_pending, checkout_stop_command_id
         FROM dispatch_ledger WHERE command_id = $1",
    ).bind(fixture.command.into_uuid()).fetch_one(&fixture.module).await?;
    assert_eq!(disposition.0, "checkout/project");
    assert_eq!(disposition.1, "applied");
    assert_eq!(disposition.2, "repository_unconfigured");
    assert!(!disposition.3);
    let state: (String, String, bool, bool) = sqlx::query_as(
        "SELECT state_kind, terminal_outcome_kind, terminal_stop_sticky, start_gate_held
         FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&fixture.core)
    .await?;
    assert_eq!(
        state,
        (
            String::from("terminal"),
            String::from("stopped"),
            false,
            true
        )
    );
    assert!(fixture.runner.steps.lock().expect("steps").is_empty());
    assert!(
        fixture
            .store
            .recover_pending_commands(&mut RepositoryWatchCommandCodec)
            .await?
            .is_empty()
    );

    // Replay a submission follow-up after configuration reintroduces the repository.
    // The retained terminal reason must still select the original non-sticky command.
    sqlx::query("UPDATE dispatch_ledger SET submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid())
        .execute(&fixture.module)
        .await?;
    fixture.sink.models = original_models;
    fixture.store = RepoWatchStore::new(fixture.module.clone());
    fixture.dispatch().await;
    assert_eq!(fixture.session().await, session);
    let stop: (Uuid, String) = sqlx::query_as(
        "SELECT checkout_stop_command_id, checkout_retired_reason FROM dispatch_ledger WHERE command_id = $1",
    ).bind(fixture.command.into_uuid()).fetch_one(&fixture.module).await?;
    assert_eq!(
        stop,
        (disposition.4, String::from("repository_unconfigured"))
    );
    let stops: i64 =
        sqlx::query_scalar("SELECT count(*) FROM session_lifecycle_command WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&fixture.core)
            .await?;
    assert_eq!(stops, 1);
    assert!(fixture.runner.steps.lock().expect("steps").is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn fork_heads_are_fetched_without_the_watched_repository_credential()
-> Result<(), Box<dyn Error>> {
    let mut fixture = CheckoutFixture::with_head_repository("contributor/project").await?;
    fixture.dispatch().await;
    let roots = SessionWorkspaceRoots::try_new(
        fixture
            .sink
            .models
            .daemon_tools()
            .expect("tools")
            .workspace_root(),
    )?;
    let checkout = git2::Repository::open(roots.derived_path(fixture.session().await))?;
    assert_eq!(
        checkout.head()?.target().expect("head").to_string(),
        fixture.head.as_str()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn dispatch_provisions_the_retained_head_at_the_git_tools_root() -> Result<(), Box<dyn Error>>
{
    let mut fixture = CheckoutFixture::new().await?;
    let remote = git2::Repository::open_bare(&fixture.runner.bare)?;
    let retained = remote.find_commit(git2::Oid::from_str(fixture.head.as_str())?)?;
    let signature = git2::Signature::now("Checkout fixture", "checkout@example.test")?;
    let advanced = remote.commit(
        Some("refs/heads/review"),
        &signature,
        &signature,
        "Branch advanced after dispatch was recorded",
        &retained.tree()?,
        &[&retained],
    )?;
    assert_ne!(advanced.to_string(), fixture.head.as_str());
    fixture.dispatch().await;
    let session = fixture.session().await;
    let tools = fixture
        .sink
        .models
        .daemon_tools()
        .expect("tool configuration");
    let root = SessionWorkspaceRoots::try_new(tools.workspace_root())?.derived_path(session);
    let repository = git2::Repository::open(&root)?;
    assert_eq!(
        repository
            .head()?
            .target()
            .expect("head commit")
            .to_string(),
        fixture.head.as_str()
    );
    assert_eq!(repository.head()?.shorthand()?, "review");
    assert_eq!(
        std::fs::read_to_string(root.join("review.txt"))?,
        "retained head\n"
    );
    let recorded: (String, String) = sqlx::query_as(
        "SELECT checkout_path, checkout_head_sha FROM dispatch_ledger WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .fetch_one(&fixture.module)
    .await?;
    assert_eq!(
        recorded,
        (String::from("."), fixture.head.as_str().to_owned())
    );
    assert!(!std::fs::read_to_string(root.join(".git/config"))?.contains(TOKEN));
    assert!(!std::fs::read_to_string(root.join(".git/config"))?.contains("extraheader"));
    let held: bool =
        sqlx::query_scalar("SELECT start_gate_held FROM session_lifecycle WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&fixture.core)
            .await?;
    assert!(held);
    let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
        .fetch_one(&fixture.core)
        .await?;
    assert_eq!(inputs, 0);
    assert_git_status(&root, tools.git_identity().clone(), session).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn clone_failure_retires_dispatch_with_only_step_and_exit_status()
-> Result<(), Box<dyn Error>> {
    let mut fixture = CheckoutFixture::new().await?;
    fixture.runner.bare = fixture.runner.bare.with_file_name("missing.git");
    fixture.dispatch().await;
    let retired: (String, String, String) = sqlx::query_as("SELECT checkout_retired_reason, checkout_failure_step, checkout_failure_status FROM dispatch_ledger WHERE command_id = $1")
        .bind(fixture.command.into_uuid()).fetch_one(&fixture.module).await?;
    assert_eq!(
        retired,
        (
            String::from("checkout_provisioning_failed"),
            String::from("clone"),
            String::from("exit:128")
        )
    );
    let state: String = sqlx::query_scalar(
        "SELECT terminal_outcome_kind FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(fixture.session().await.into_uuid())
    .fetch_one(&fixture.core)
    .await?;
    assert_eq!(state, "stopped");
    fixture.dispatch().await;
    assert_eq!(*fixture.runner.steps.lock().expect("steps"), ["clone"]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn recovery_fetches_an_existing_checkout_without_cloning_again() -> Result<(), Box<dyn Error>>
{
    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    // Models a crash after filesystem provisioning but before ledger settlement.
    sqlx::query("UPDATE dispatch_ledger SET checkout_path = NULL, checkout_head_sha = NULL, submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid()).execute(&fixture.module).await?;
    fixture.change_workspace_root()?;
    fixture.dispatch().await;
    assert_eq!(
        *fixture.runner.steps.lock().expect("steps"),
        ["clone", "fetch", "checkout", "fetch", "checkout"]
    );
    let head: String =
        sqlx::query_scalar("SELECT checkout_head_sha FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    assert_eq!(head, fixture.head.as_str());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn a_symlinked_workspace_parent_retires_dispatch_before_git_runs()
-> Result<(), Box<dyn Error>> {
    let mut fixture = CheckoutFixture::new().await?;
    let roots = SessionWorkspaceRoots::try_new(
        fixture
            .sink
            .models
            .daemon_tools()
            .expect("tools")
            .workspace_root(),
    )?;
    let derived = roots.derived_path(SessionId::from_uuid(Uuid::now_v7()));
    std::os::unix::fs::symlink(
        fixture._files.path(),
        derived.parent().expect("derived parent"),
    )?;
    fixture.dispatch().await;
    let failure: (String, String) = sqlx::query_as("SELECT checkout_failure_step, checkout_failure_status FROM dispatch_ledger WHERE command_id = $1")
        .bind(fixture.command.into_uuid()).fetch_one(&fixture.module).await?;
    assert_eq!(
        failure,
        (String::from("workspace"), String::from("not_started"))
    );
    assert!(fixture.runner.steps.lock().expect("steps").is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn terminal_session_removes_its_checkout_without_following_tracked_symlinks()
-> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;
    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    let root = fixture.root(session);
    let outside = fixture._files.path().join("outside");
    std::fs::create_dir(&outside)?;
    std::fs::write(outside.join("keep.txt"), "outside the checkout")?;
    std::os::unix::fs::symlink(&outside, root.join("outside"))?;
    sqlx::query("UPDATE dispatch_ledger SET submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid())
        .execute(&fixture.module)
        .await?;
    scavenge_checkouts(&fixture.store, &fixture.core)
        .await
        .expect("active checkout retained");
    assert!(root.join(".git").is_dir());
    fixture.stop(session).await;
    scavenge_checkouts(&fixture.store, &fixture.core)
        .await
        .expect("terminal checkout removed");
    assert!(!root.exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("keep.txt"))?,
        "outside the checkout"
    );
    let removed: bool =
        sqlx::query_scalar("SELECT checkout_removed FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    assert!(removed);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn provisioned_checkout_requires_a_retained_location() -> Result<(), Box<dyn Error>> {
    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let error = sqlx::query(
        "UPDATE dispatch_ledger SET checkout_workspace_root = NULL, checkout_session_id = NULL WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .execute(&fixture.module)
    .await
    .expect_err("provisioned checkout must retain its cleanup location");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cleanup_restores_owner_permissions_on_root_and_nested_directories()
-> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;
    use std::os::unix::fs::PermissionsExt;

    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    let root = fixture.root(session);
    let nested = root.join("unreadable");
    std::fs::create_dir(&nested)?;
    std::fs::write(nested.join("remove.txt"), "checkout contents")?;
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o000))?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000))?;
    fixture.stop(session).await;
    scavenge_checkouts(&fixture.store, &fixture.core)
        .await
        .expect("cleanup traverses unreadable checkout directories");
    assert!(!root.exists());
    let removed: bool =
        sqlx::query_scalar("SELECT checkout_removed FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    assert!(removed, "successful removal must settle on the ledger");
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn disabled_runtime_scavenges_checkouts_without_submitting_pending_commands()
-> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_runtime::{RepositoryWatchRuntime, RepositoryWatchServices};

    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    let root = fixture.root(session);
    sqlx::query("UPDATE dispatch_ledger SET submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid())
        .execute(&fixture.module)
        .await?;
    let models = HubModelConfiguration::parse(
        &fixture.catalog.replace("enabled = true", "enabled = false"),
    )?;
    let templates = signalboxd::SessionTemplateConfiguration::read(
        &fixture._files.path().join("templates.toml"),
        || None,
        &models,
    )?;
    let runtime = RepositoryWatchRuntime::new(
        fixture.module.clone(),
        models.repository_watch().cloned(),
        RepositoryWatchServices {
            core_pool: fixture.core.clone(),
            models: Arc::new(models),
            templates: Arc::new(templates),
            eligibility_nudge: fixture.sink.eligibility_nudge.clone(),
            tool_dispatch_gate: fixture.sink.tool_dispatch_gate.clone(),
        },
    )
    .await
    .expect("prepare disabled runtime");
    let (shutdown, stopped) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(runtime.run(stopped));
    assert!(root.join(".git").is_dir());
    fixture.stop(session).await;
    let cleanup = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let removed: bool = sqlx::query_scalar(
                "SELECT checkout_removed FROM dispatch_ledger WHERE command_id = $1",
            )
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
            if removed {
                return Ok::<_, sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    shutdown.send(true)?;
    worker.await?.expect("disabled runtime shuts down cleanly");
    cleanup??;
    assert!(!root.exists());
    let pending: bool = sqlx::query_scalar(
        "SELECT submission_pending FROM mod_repo_watch.dispatch_ledger WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .fetch_one(&fixture.core)
    .await?;
    assert!(
        pending,
        "disabled runtime must not submit retained commands"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn startup_scavenges_interrupted_retirements_without_repository_watch_configuration()
-> Result<(), Box<dyn Error>> {
    assert_startup_scavenges_interrupted_checkout(true).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn startup_scavenges_interrupted_terminal_checkouts_without_repository_watch_configuration()
-> Result<(), Box<dyn Error>> {
    assert_startup_scavenges_interrupted_checkout(false).await
}

async fn assert_startup_scavenges_interrupted_checkout(
    retired: bool,
) -> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;
    let mut fixture = CheckoutFixture::new().await?;
    if retired {
        fixture.runner.bare = fixture.runner.bare.with_file_name("missing.git");
    }
    fixture.submit_without_lifecycle_settlement().await;
    let session = fixture
        .store
        .dispatch_checkout(fixture.command)
        .await?
        .expect("checkout row")
        .location
        .expect("retained location")
        .session;
    // The checkout disposition survived; command follow-up completion did not.
    sqlx::query("UPDATE dispatch_ledger SET submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid())
        .execute(&fixture.module)
        .await?;
    let root = fixture.root(session);
    assert!(root.is_dir());
    let restarted = RepoWatchStore::new(fixture.module.clone());
    if !retired {
        scavenge_checkouts(&restarted, &fixture.core)
            .await
            .expect("active checkout retained before lifecycle settlement");
        assert!(root.join(".git").is_dir());
        fixture.stop(session).await;
    }
    let unsettled: bool = sqlx::query_scalar(
        "SELECT created_session_id IS NULL FROM dispatch_ledger WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .fetch_one(&fixture.module)
    .await?;
    assert!(unsettled);
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("startup scavenges retired checkout");
    assert!(!root.exists());
    let flags: (bool, bool, bool) = sqlx::query_as(
        "SELECT submission_pending, checkout_removed, created_session_id IS NULL FROM dispatch_ledger WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .fetch_one(&fixture.module)
    .await?;
    assert_eq!(flags, (true, true, true));
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("repeated startup is idempotent");
    assert!(restarted.checkout_removal_candidates().await?.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cleanup_uses_the_provisioning_root_after_configuration_changes()
-> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;
    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    let original = fixture.root(session);
    fixture.change_workspace_root()?;
    let replacement = fixture.root(session);
    assert_ne!(original, replacement);
    std::fs::create_dir_all(&replacement)?;
    std::fs::write(replacement.join("keep"), "new workspace contents")?;
    fixture.stop(session).await;
    let restarted = RepoWatchStore::new(fixture.module.clone());
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("original workspace removed");
    assert!(!original.exists());
    assert_eq!(
        std::fs::read_to_string(replacement.join("keep"))?,
        "new workspace contents"
    );
    assert!(restarted.checkout_removal_candidates().await?.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cleanup_rejects_a_symlink_replacing_the_session_root() -> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;
    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    let root = fixture.root(session);
    let retained = fixture._files.path().join("retained");
    std::fs::rename(&root, &retained)?;
    std::os::unix::fs::symlink(&retained, &root)?;
    fixture.stop(session).await;
    scavenge_checkouts(&fixture.store, &fixture.core)
        .await
        .expect("unsafe removal remains pending");
    assert!(std::fs::symlink_metadata(&root)?.is_symlink());
    assert_eq!(
        std::fs::read_to_string(retained.join("review.txt"))?,
        "retained head\n"
    );
    assert_eq!(fixture.store.checkout_removal_candidates().await?.len(), 1);
    Ok(())
}

async fn assert_git_status(
    root: &Path,
    identity: signalbox_tools_git::GitIdentity,
    session: SessionId,
) {
    use signalbox_application::*;
    use signalbox_domain::{
        ContextFrontierId, ModelCallId, ToolAttemptId, ToolRequestId, TurnAttemptId, TurnId,
    };
    let tools = signalbox_tools_git::LocalGitTools::try_new(
        signalbox_tools_workspace::LocalWorkspaceFileSystem,
        root,
        identity,
    )
    .expect("checkout admits built-in Git");
    let (catalog, executor) = tools.into_parts();
    let (executor, recorded) = RecordingToolExecutor::new(executor);
    let batch = prepared_single_attempt_batch(
        PreparedAttemptIdentities {
            session,
            turn: TurnId::from_uuid(Uuid::now_v7()),
            producing_call: ModelCallId::from_uuid(Uuid::now_v7()),
            request: ToolRequestId::from_uuid(Uuid::now_v7()),
            attempt: ToolAttemptId::from_uuid(Uuid::now_v7()),
            issuing_turn_attempt: TurnAttemptId::from_uuid(Uuid::now_v7()),
            frontier: ContextFrontierId::from_uuid(Uuid::now_v7()),
        },
        PreparedAttemptProposal {
            name: signalbox_domain::ToolName::try_new(String::from("git_status"))
                .expect("tool name"),
            arguments: signalbox_domain::NormalizedToolArguments::try_from_provider_text(
                String::from("{}"),
            )
            .expect("arguments"),
            effect_class: signalbox_domain::ToolEffectClass::EffectFree,
            approval: PreparedAttemptApproval::PolicyAuto,
        },
    );
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        FixtureToolExecutionTransaction::new(
            batch.clone(),
            FixtureTransactionFailures {
                domain_rejection: signalbox_tools_git::LocalGitExecutorError,
                declined_crash_classification: signalbox_tools_git::LocalGitExecutorError,
            },
        ),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );
    service
        .execute(batch.session(), batch.turn())
        .await
        .expect("execute git_status");
    assert!(matches!(
        recorded.take(),
        Some(ToolExecutorEvidence::CompletedText(_))
    ));
}
