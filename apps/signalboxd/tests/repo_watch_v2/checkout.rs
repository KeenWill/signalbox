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
            if argument == "https://github.com/checkout/project.git" {
                *argument = self.bare.clone().into_os_string();
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
}

impl CheckoutFixture {
    async fn new() -> Result<Self, Box<dyn Error>> {
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
        let models = HubModelConfiguration::parse(&format!(
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
        ))?;
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
                            head_repository: repository.clone(),
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
                steps: Arc::default(),
            },
            command,
            head,
        })
    }

    async fn dispatch(&mut self) {
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
